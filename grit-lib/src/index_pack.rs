//! Install a received packfile into the object store (`index-pack` path).
//!
//! Fetch and clone keep the negotiated pack on disk (`.pack` + `.idx`) instead of
//! exploding every object into loose storage. Small receive-pack pushes may still
//! use [`crate::unpack_objects`] via [`crate::receive_pack::should_use_unpack_objects`].

use std::collections::HashSet;
use std::path::Path;

use crate::error::{Error, Result};
use crate::hash::Parallelism;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::pack::{clear_pack_cache, verify_pack_and_collect, write_v2_pack_index_with_trailer};
use crate::transfer::fix_thin_pack;
use crate::unpack_objects::{pack_index_records_with_threads, PackIndexRecord};

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
            crate::config::ConfigSet::load(Some(git_dir), true)
                .map(|c| c.pack_index_parallelism())
                .unwrap_or_else(|_| Parallelism::resolve(None))
        } else {
            Parallelism::resolve(None)
        }
    }
}

/// Ingest a pack received from fetch/clone by installing it under `objects/pack/`.
///
/// Returns the set of object ids recorded in the pack index.
pub fn ingest_received_pack(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<HashSet<ObjectId>> {
    if pack.is_empty() {
        return Ok(HashSet::new());
    }
    if pack.len() < 12 || &pack[0..4] != b"PACK" {
        return Err(Error::CorruptObject(
            "received data is not a pack stream".to_owned(),
        ));
    }
    install_pack_bytes(pack, odb, opts)
}

/// Write `pack` into `odb`'s `objects/pack/` directory with a v2 index.
///
/// Builds under temporary names and publishes the `.pack`/`.idx` pair atomically.
pub fn install_pack_bytes(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<HashSet<ObjectId>> {
    let pack = if opts.fix_thin {
        fix_thin_pack(pack, odb)?
    } else {
        pack
    };
    let hb = odb.hash_algo().len();
    if pack.len() < 12 + hb {
        return Err(Error::CorruptObject("pack too small".to_owned()));
    }
    let pack_hash = ObjectId::from_bytes(&pack[pack.len() - hb..])?;
    let stem = format!("pack-{}", pack_hash.to_hex());
    let pack_dir = odb.objects_dir().join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    let pack_path = pack_dir.join(format!("{stem}.pack"));
    let idx_path = pack_dir.join(format!("{stem}.idx"));
    let stage = pack_dir.join(format!(".{stem}-install"));
    let stage_pack = stage.join(format!("{stem}.pack"));
    let stage_idx = stage.join(format!("{stem}.idx"));

    cleanup_stale_install_stage(&stage);

    let install_result = (|| -> Result<HashSet<ObjectId>> {
        std::fs::create_dir_all(&stage).map_err(Error::Io)?;
        std::fs::write(&stage_pack, &pack).map_err(Error::Io)?;
        let records = pack_index_records_with_threads(&pack, odb, opts.index_parallelism(odb))?;
        let oids: HashSet<ObjectId> = records.iter().map(|r| r.oid).collect();
        let entries: Vec<(ObjectId, u64, u32)> = records
            .into_iter()
            .map(|PackIndexRecord { oid, offset, crc32 }| (oid, offset, crc32))
            .collect();
        let trailer = &pack[pack.len() - hb..];
        write_v2_pack_index_with_trailer(&stage_idx, &entries, trailer, hb)?;
        // Same validation `git index-pack` performs before the pack is usable (CRCs, offsets, hashes).
        verify_pack_and_collect(&stage_idx)?;
        std::fs::rename(&stage_pack, &pack_path).map_err(Error::Io)?;
        std::fs::rename(&stage_idx, &idx_path).map_err(|e| {
            let _ = std::fs::remove_file(&pack_path);
            Error::Io(e)
        })?;
        let _ = std::fs::remove_dir(&stage);
        Ok(oids)
    })();

    if install_result.is_err() {
        cleanup_stale_install_stage(&stage);
        let _ = std::fs::remove_file(&pack_path);
        let _ = std::fs::remove_file(&idx_path);
    }
    let oids = install_result?;
    clear_pack_cache();
    Ok(oids)
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

    fn pack_dir_entries(pack_dir: &Path) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(pack_dir)
            .expect("read pack dir")
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect()
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
