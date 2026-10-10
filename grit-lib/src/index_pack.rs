//! Install a received packfile into the object store (`index-pack` path).
//!
//! Fetch and clone keep the negotiated pack on disk (`.pack` + `.idx`) instead of
//! exploding every object into loose storage. Small receive-pack pushes may still
//! use [`crate::unpack_objects`] via [`crate::receive_pack::should_use_unpack_objects`].

use std::collections::HashSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::hash::{verify_trailer, Parallelism};
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::pack::{verify_pack_and_collect, write_v2_pack_index_with_trailer};
use crate::pack_map::PackData;
use crate::transfer::fix_thin_pack_path;
use crate::unpack_objects::{pack_index_records_with_threads, PackIndexRecord};

/// Result of installing a pack under `objects/pack/`.
#[derive(Debug, Clone)]
pub struct IngestedPack {
    /// Object ids recorded in the new pack index.
    pub object_ids: HashSet<ObjectId>,
    /// Absolute path to the published `pack-*.pack` file.
    pub pack_path: PathBuf,
}

/// Options controlling how a received pack is ingested.
#[derive(Debug, Clone, Default)]
pub struct IngestPackOptions {
    /// When true, append missing ref-delta bases from `odb` before indexing
    /// (matches `git index-pack --fix-thin`).
    pub fix_thin: bool,
    /// Worker threads for index-pack hashing (`None`/`Some(0)` → read `pack.threads` or all CPUs).
    pub threads: Option<usize>,
}

impl IngestPackOptions {
    fn index_parallelism(&self, odb: &Odb) -> Parallelism {
        if let Some(n) = self.threads {
            Parallelism::resolve(Some(n))
        } else if let Some(git_dir) = odb.config_git_dir() {
            crate::config::ConfigSet::load(
                &crate::environment::Environment::empty(),
                Some(git_dir),
                true,
            )
            .map(|c| c.pack_index_parallelism())
            .unwrap_or_else(|_| Parallelism::resolve(None))
        } else {
            Parallelism::resolve(None)
        }
    }
}

/// Ingest a pack received from fetch/clone by installing it under `objects/pack/`.
///
/// Returns the installed pack identity and indexed object ids.
pub fn ingest_received_pack(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    if pack.is_empty() {
        return Ok(IngestedPack {
            object_ids: HashSet::new(),
            pack_path: PathBuf::new(),
        });
    }
    if pack.len() < 12 || &pack[0..4] != b"PACK" {
        return Err(Error::CorruptObject(
            "received data is not a pack stream".to_owned(),
        ));
    }
    verify_trailer(odb.hash_algo(), &pack).map_err(|e| {
        Error::CorruptObject(format!(
            "received pack checksum mismatch (download may be incomplete): {e}"
        ))
    })?;
    install_pack_bytes(pack, odb, opts)
}

/// Ingest a pack already written to disk (streaming fetch/clone receive path).
///
/// # Errors
///
/// Same as [`install_pack_path`].
pub fn ingest_received_pack_path(
    pack_path: PathBuf,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    if !pack_path.is_file() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("received pack file missing: {}", pack_path.display()),
        )));
    }
    install_pack_path(&pack_path, odb, opts)
}

/// Write `pack` into `odb`'s `objects/pack/` directory with a v2 index.
///
/// Builds under temporary names and publishes the `.pack`/`.idx` pair atomically.
pub fn install_pack_bytes(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    odb.require_files_primary("index_pack")?;
    let pack_dir = odb.objects_dir().join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    static INSTALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = INSTALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = pack_dir.join(format!("tmp_install_{}_{seq}", std::process::id()));
    std::fs::write(&tmp, &pack).map_err(Error::Io)?;
    match install_pack_path(&tmp, odb, opts) {
        Ok(ingested) => Ok(ingested),
        Err(e) => {
            let _ = std::fs::remove_file(&tmp);
            Err(e)
        }
    }
}

/// Install a pack from `pack_path` into `odb`'s `objects/pack/` directory.
///
/// The file is moved into place when possible; callers may pass a temp receive
/// path under `objects/pack/`.
pub fn install_pack_path(
    pack_path: &Path,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    odb.require_files_primary("index_pack")?;
    let mut owned = pack_path.to_path_buf();
    if opts.fix_thin {
        owned = fix_thin_pack_path(&owned, odb)?;
    }
    let hb = odb.hash_algo().len();
    let meta = std::fs::metadata(&owned).map_err(Error::Io)?;
    let len = meta.len();
    if len < 12 + hb as u64 {
        return Err(Error::CorruptObject("pack too small".to_owned()));
    }
    let mut header = [0u8; 12];
    {
        use std::io::Read;
        let mut f = std::fs::File::open(&owned).map_err(Error::Io)?;
        f.read_exact(&mut header).map_err(Error::Io)?;
    }
    if &header[0..4] != b"PACK" {
        return Err(Error::CorruptObject(
            "received data is not a pack stream".to_owned(),
        ));
    }
    let mut trailer = vec![0u8; hb];
    {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&owned).map_err(Error::Io)?;
        f.seek(SeekFrom::End(-(hb as i64))).map_err(Error::Io)?;
        f.read_exact(&mut trailer).map_err(Error::Io)?;
    }
    let pack_hash = ObjectId::from_bytes(&trailer)?;
    let stem = format!("pack-{}", pack_hash.to_hex());
    let pack_dir = odb.objects_dir().join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    let final_pack = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    let stage = pack_dir.join(format!(".{stem}-install"));
    let stage_pack = stage.join(format!("{stem}.pack"));
    let stage_idx = stage.join(format!("{stem}.idx"));

    cleanup_stale_install_stage(&stage);

    let install_result = (|| -> Result<IngestedPack> {
        std::fs::create_dir_all(&stage).map_err(Error::Io)?;
        if std::fs::rename(&owned, &stage_pack).is_err() {
            std::fs::copy(&owned, &stage_pack).map_err(Error::Io)?;
            let _ = std::fs::remove_file(&owned);
        }
        let indexed = PackData::open(&stage_pack)?;
        let (records, trailer) = {
            let pack_bytes: &[u8] = indexed.deref();
            verify_trailer(odb.hash_algo(), pack_bytes).map_err(|e| {
                Error::CorruptObject(format!(
                    "received pack checksum mismatch (download may be incomplete): {e}"
                ))
            })?;
            let records =
                pack_index_records_with_threads(pack_bytes, odb, opts.index_parallelism(odb))?;
            let trailer = pack_bytes[pack_bytes.len() - hb..].to_vec();
            (records, trailer)
        };
        drop(indexed);
        let oids: HashSet<ObjectId> = records.iter().map(|r| r.oid).collect();
        let entries: Vec<(ObjectId, u64, u32)> = records
            .into_iter()
            .map(|PackIndexRecord { oid, offset, crc32 }| (oid, offset, crc32))
            .collect();
        write_v2_pack_index_with_trailer(&stage_idx, &entries, &trailer, hb)?;
        verify_pack_and_collect(&stage_idx)?;
        std::fs::rename(&stage_pack, &final_pack).map_err(Error::Io)?;
        std::fs::rename(&stage_idx, &idx_path).map_err(|e| {
            let _ = std::fs::remove_file(&final_pack);
            Error::Io(e)
        })?;
        let _ = std::fs::remove_dir(&stage);
        Ok(IngestedPack {
            object_ids: oids,
            pack_path: final_pack.clone(),
        })
    })();

    if install_result.is_err() {
        cleanup_stale_install_stage(&stage);
        let _ = std::fs::remove_file(&final_pack);
        let _ = std::fs::remove_file(&idx_path);
        let _ = std::fs::remove_file(&owned);
    }
    let ingested = install_result?;
    odb.invalidate_packs();
    Ok(ingested)
}

fn cleanup_stale_install_stage(stage: &Path) {
    if !stage.exists() {
        return;
    }
    if let Ok(entries) = std::fs::read_dir(stage) {
        for entry in entries.flatten() {
            let _ = std::fs::remove_file(entry.path());
        }
    }
    let _ = std::fs::remove_dir(stage);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use crate::pack::read_pack_index;
    use std::process::Command;

    fn git_index_pack(pack: &[u8]) -> (tempfile::TempDir, Vec<u8>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("in.pack");
        std::fs::write(&pack_path, pack).expect("write");
        let out = Command::new("git")
            .current_dir(dir.path())
            .args(["index-pack", "-v", &pack_path.to_string_lossy()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git index-pack");
        assert!(
            out.status.success(),
            "git index-pack: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let indexed = std::fs::read(dir.path().join("in.pack")).expect("read indexed pack");
        (dir, indexed)
    }

    fn single_blob_pack(data: &[u8]) -> Vec<u8> {
        use crate::transfer::{build_pack, PackBuildOptions};
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let oid = odb
            .write(ObjectKind::Blob, data)
            .expect("write loose blob for pack build");
        build_pack(
            &odb,
            &[oid],
            &[],
            &PackBuildOptions {
                delta: false,
                ..Default::default()
            },
        )
        .expect("build pack")
    }

    /// Regression for large-clone failures (#889): parallel index-pack must skip
    /// 0-byte zlib members instead of mis-scanning the stream (`unknown packed-object type 0`).
    #[test]
    fn parallel_index_pack_zero_byte_blob_among_many() {
        use crate::hash::Parallelism;
        use crate::unpack_objects::pack_index_records_with_threads;
        use grit_test_support::objects::{HashAlgo as FixtureAlgo, ObjectKind as PK, PackBuilder};

        let mut pb = PackBuilder::new(FixtureAlgo::Sha1);
        for i in 0..200 {
            pb.add_full(PK::Blob, format!("payload-{i}\n").as_bytes());
        }
        pb.add_full(PK::Blob, b"");
        for i in 200..400 {
            pb.add_full(PK::Blob, format!("after-empty-{i}\n").as_bytes());
        }
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        for threads in [1_usize, 8] {
            let records =
                pack_index_records_with_threads(&pack, &odb, Parallelism::resolve(Some(threads)))
                    .unwrap_or_else(|e| panic!("threads={threads}: {e}"));
            assert_eq!(records.len(), 401, "threads={threads}");
        }
    }

    #[test]
    fn install_rejects_truncated_pack_before_object_scan() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut pack = single_blob_pack(b"truncated-before-index");
        pack.truncate(pack.len().saturating_sub(8));
        let err = install_pack_bytes(
            pack,
            &odb,
            &IngestPackOptions {
                fix_thin: false,
                ..Default::default()
            },
        )
        .expect_err("truncated pack");
        let msg = err.to_string();
        assert!(
            msg.contains("checksum mismatch") || msg.contains("truncated"),
            "expected checksum/truncated error, got {msg}"
        );
    }

    fn pack_dir_entries(pack_dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(pack_dir)
            .expect("read pack dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect()
    }

    /// Manual RSS: `GRIT_CARGO_PACK=/path/to.pack cargo test -p grit-lib --release rss_mmap_index_records -- --ignored --exact`
    #[test]
    #[ignore = "manual RSS harness (GRIT_CARGO_PACK)"]
    fn rss_mmap_index_records() {
        use crate::hash::Parallelism;
        use std::env;
        let path = env::var("GRIT_CARGO_PACK").expect("GRIT_CARGO_PACK");
        let mapped = PackData::open(Path::new(&path)).expect("mmap");
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let _records =
            pack_index_records_with_threads(mapped.deref(), &odb, Parallelism::resolve(Some(4)))
                .expect("index records");
    }

    #[test]
    fn install_pack_matches_git_index_pack_layout() {
        let blob = b"clone pack retention fixture\n";
        let pack = single_blob_pack(blob);
        let (_git_dir, git_pack) = git_index_pack(&pack);

        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        install_pack_bytes(
            git_pack,
            &odb,
            &IngestPackOptions {
                fix_thin: false,
                ..Default::default()
            },
        )
        .expect("install");

        let pack_dir = tmp.path().join("pack");
        let packs: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("read pack dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(packs.len(), 1, "expected one pack file");
        let loose_count = std::fs::read_dir(tmp.path())
            .expect("read objects")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|s| s.len() == 2 && s.chars().all(|c| c.is_ascii_hexdigit()))
            })
            .count();
        assert_eq!(loose_count, 0, "install must not create loose objects");

        let oid = odb.hash(ObjectKind::Blob, blob);
        let obj = odb.read(&oid).expect("read from pack");
        assert_eq!(obj.data.as_slice(), blob);
    }

    fn run_git(cwd: &Path, args: &[&str]) {
        let out = Command::new("/usr/bin/git")
            .args(args)
            .current_dir(cwd)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} in {}: {}",
            cwd.display(),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn local_fetch_ingest_with_fix_thin_from_bare_remote() {
        use crate::objects::ObjectId;
        use crate::transfer::{build_pack, open_odb, PackBuildOptions};

        let tmp = tempfile::tempdir().expect("tempdir");
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).expect("work");
        run_git(&work, &["init", "-q", "-b", "main", "."]);
        run_git(&work, &["commit", "-q", "--allow-empty", "-m", "c1"]);
        let bare = tmp.path().join("repo.git");
        run_git(
            tmp.path(),
            &[
                "clone",
                "-q",
                "--bare",
                work.to_str().unwrap(),
                bare.to_str().unwrap(),
            ],
        );

        let consumer = tmp.path().join("consumer");
        std::fs::create_dir_all(&consumer).expect("consumer");
        run_git(&consumer, &["init", "-q", "-b", "main", "."]);
        let local_git = consumer.join(".git");
        let local_odb = open_odb(&local_git);
        let remote_odb = open_odb(&bare);

        let tip_out = Command::new("/usr/bin/git")
            .args(["rev-parse", "refs/heads/main"])
            .current_dir(&bare)
            .output()
            .expect("rev-parse");
        assert!(tip_out.status.success());
        let tip = ObjectId::from_hex(String::from_utf8_lossy(&tip_out.stdout).trim()).expect("oid");
        let pack =
            build_pack(&remote_odb, &[tip], &[], &PackBuildOptions::default()).expect("build_pack");
        assert!(
            !crate::unpack_objects::pack_is_thin(&pack, remote_odb.hash_algo()),
            "whole-object local fetch pack must not be classified as thin"
        );
        let (_git_dir, _indexed) = git_index_pack(&pack);
        install_pack_bytes(
            pack,
            &local_odb,
            &IngestPackOptions {
                fix_thin: true,
                threads: Some(1),
                ..Default::default()
            },
        )
        .expect("ingest like fetch_local serial index-pack");
        assert!(local_odb.exists(&tip));
    }

    #[test]
    fn fetch_ingest_keeps_pack_despite_receive_unpacklimit_zero() {
        let blob = b"unpacklimit must not apply to fetch ingest\n";
        let pack = single_blob_pack(blob);
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).expect("objects");
        std::fs::write(git_dir.join("config"), "[receive]\n\tunpackLimit = 0\n").expect("config");
        let objects = git_dir.join("objects");
        let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
        install_pack_bytes(
            pack,
            &odb,
            &IngestPackOptions {
                fix_thin: false,
                ..Default::default()
            },
        )
        .expect("install");
        let pack_dir = git_dir.join("objects").join("pack");
        let packs: Vec<_> = std::fs::read_dir(&pack_dir)
            .expect("pack dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .collect();
        assert_eq!(packs.len(), 1, "fetch ingest must index pack, not unpack");
    }

    #[test]
    fn v2_index_large_offset_roundtrip() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let pack_path = tmp.path().join("large.pack");
        let pack = single_blob_pack(b"large offset idx fixture");
        std::fs::write(&pack_path, &pack).expect("write pack");
        let idx_path = pack_path.with_extension("idx");
        let fake_oid = ObjectId::from_hex("aabbccddeeff00112233445566778899aabbccdd").unwrap();
        let huge_off = 1u64 << 31;
        write_v2_pack_index_with_trailer(
            &idx_path,
            &[(fake_oid, huge_off, 0x1234_5678)],
            &pack[pack.len() - 20..],
            20,
        )
        .expect("write idx");
        let idx = read_pack_index(&idx_path).expect("read idx");
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.offset_at(0), huge_off);
    }

    fn pack_with_junk_before_trailer(valid: &[u8], junk: &[u8]) -> Vec<u8> {
        use crate::objects::HashAlgo;

        let algo = HashAlgo::Sha1;
        let hb = algo.len();
        let body = &valid[..valid.len().checked_sub(hb).expect("pack trailer")];
        let mut body_with_junk = body.to_vec();
        body_with_junk.extend_from_slice(junk);
        let trailer = algo.digest(&body_with_junk);
        body_with_junk.extend_from_slice(trailer.as_bytes());
        body_with_junk
    }

    fn git_index_pack_strict_rejects(pack: &[u8]) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("in.pack");
        std::fs::write(&pack_path, pack).expect("write pack");
        let out = Command::new("/usr/bin/git")
            .current_dir(dir.path())
            .args([
                "index-pack",
                "--strict",
                pack_path.to_string_lossy().as_ref(),
            ])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git index-pack --strict");
        assert!(
            !out.status.success(),
            "git index-pack --strict must reject junk before trailer: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn rejects_extra_bytes_before_pack_trailer() {
        use crate::hash::Parallelism;
        use crate::unpack_objects::pack_index_records_with_threads;

        let valid = single_blob_pack(b"one blob\n");
        let corrupted = pack_with_junk_before_trailer(&valid, b"JUNK");
        git_index_pack_strict_rejects(&corrupted);

        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());

        for threads in [1_usize, 4] {
            let err = pack_index_records_with_threads(
                &corrupted,
                &odb,
                Parallelism::resolve(Some(threads)),
            )
            .expect_err("index-pack hashing");
            assert!(
                matches!(err, Error::CorruptObject(_)),
                "threads={threads}: {err:?}"
            );
        }

        for threads in [1_usize, 4] {
            let err = install_pack_bytes(
                corrupted.clone(),
                &odb,
                &IngestPackOptions {
                    fix_thin: false,
                    threads: Some(threads),
                    ..Default::default()
                },
            )
            .expect_err("install pack");
            assert!(
                matches!(err, Error::CorruptObject(_)),
                "install threads={threads}: {err:?}"
            );
        }
    }

    #[test]
    fn install_failure_leaves_no_published_pack_or_index() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut pack = single_blob_pack(b"truncated");
        pack.truncate(pack.len().saturating_sub(8));
        let err = install_pack_bytes(
            pack,
            &odb,
            &IngestPackOptions {
                fix_thin: false,
                ..Default::default()
            },
        )
        .expect_err("truncated pack must fail");
        assert!(
            matches!(err, Error::CorruptObject(_)),
            "expected corrupt object, got {err:?}"
        );
        let pack_dir = tmp.path().join("pack");
        if pack_dir.exists() {
            let entries = pack_dir_entries(&pack_dir);
            assert!(
                entries
                    .iter()
                    .all(|p| p.extension().is_some_and(|e| e == "pack" || e == "idx")
                        || p.file_name()
                            .and_then(|n| n.to_str())
                            .is_some_and(|n| n.starts_with('.') && n.ends_with("-install"))),
                "must not publish partial .pack/.idx, got {entries:?}"
            );
        }
    }
}
