//! Install a received packfile into the object store (`index-pack` path).
//!
//! Fetch and clone keep the negotiated pack on disk (`.pack` + `.idx`) instead of
//! exploding every object into loose storage. Small receive-pack pushes may still
//! use [`crate::unpack_objects`] via [`crate::receive_pack::should_use_unpack_objects`].

use std::collections::HashSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::error::{Error, Result};
use crate::hash::{verify_trailer, Parallelism};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::pack::{verify_pack_and_collect, write_v2_pack_index_with_trailer};
use crate::pack_map::PackData;
use crate::progress::ProgressSink;
use crate::transfer::fix_thin_pack_path;
use crate::unpack_objects::PackIndexRecord;

pub use crate::pack_index_build::{
    build_pack_index_records, build_pack_index_records_with_progress,
};

static INGEST_THREAD_RECORDING: AtomicBool = AtomicBool::new(false);
static LAST_RESOLVED_INGEST_THREADS: AtomicUsize = AtomicUsize::new(0);

/// Enable recording of resolved index-pack worker counts (integration tests only).
pub fn set_ingest_thread_recording_for_tests(enabled: bool) {
    INGEST_THREAD_RECORDING.store(enabled, Ordering::SeqCst);
    if !enabled {
        LAST_RESOLVED_INGEST_THREADS.store(0, Ordering::SeqCst);
    }
}

/// Last resolved worker count from pack ingest while recording is enabled.
pub fn last_resolved_ingest_threads_for_tests() -> Option<usize> {
    if !INGEST_THREAD_RECORDING.load(Ordering::SeqCst) {
        return None;
    }
    let raw = LAST_RESOLVED_INGEST_THREADS.load(Ordering::SeqCst);
    (raw > 0).then(|| raw - 1)
}

fn record_resolved_ingest_threads(threads: usize) {
    if INGEST_THREAD_RECORDING.load(Ordering::SeqCst) {
        LAST_RESOLVED_INGEST_THREADS.store(threads.saturating_add(1), Ordering::SeqCst);
    }
}

/// Per-type object counts for a pack, tallied while it is indexed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ObjectKindCounts {
    /// Commit objects.
    pub commits: u64,
    /// Tree objects.
    pub trees: u64,
    /// Blob objects.
    pub blobs: u64,
    /// Annotated tag objects.
    pub tags: u64,
}

impl ObjectKindCounts {
    /// Count one object of `kind`.
    pub fn add(&mut self, kind: ObjectKind) {
        match kind {
            ObjectKind::Commit => self.commits += 1,
            ObjectKind::Tree => self.trees += 1,
            ObjectKind::Blob => self.blobs += 1,
            ObjectKind::Tag => self.tags += 1,
        }
    }

    /// Sum of every type.
    #[must_use]
    pub fn total(&self) -> u64 {
        self.commits + self.trees + self.blobs + self.tags
    }
}

impl FromIterator<ObjectKind> for ObjectKindCounts {
    fn from_iter<I: IntoIterator<Item = ObjectKind>>(iter: I) -> Self {
        let mut counts = Self::default();
        for kind in iter {
            counts.add(kind);
        }
        counts
    }
}

/// Result of installing a pack under `objects/pack/`.
#[derive(Debug, Clone)]
pub struct IngestedPack {
    /// Object ids recorded in the new pack index.
    pub object_ids: HashSet<ObjectId>,
    /// Absolute path to the published `pack-*.pack` file.
    pub pack_path: PathBuf,
    /// How many objects of each type the pack holds.
    pub object_counts: ObjectKindCounts,
}

/// Options controlling how a received pack is ingested.
#[derive(Debug, Clone)]
pub struct IngestPackOptions {
    /// When true, append missing ref-delta bases from `odb` before indexing
    /// (matches `git index-pack --fix-thin`).
    pub fix_thin: bool,
    /// Worker threads for index-pack hashing (`None`/`Some(0)` → read `pack.threads` or all CPUs).
    pub threads: Option<usize>,
    /// Skip the redundant post-index [`verify_pack_and_collect`] pass after the
    /// pack trailer was validated during indexing. Does not disable trailer checks.
    pub skip_post_index_verify: bool,
    /// Merged config from the fetch/clone environment (includes command-scope `GIT_CONFIG_*`).
    pub merged_config: Option<crate::config::ConfigSet>,
    /// Environment used when [`Self::merged_config`] is unset and config is loaded from disk.
    pub environment: crate::environment::Environment,
}

impl Default for IngestPackOptions {
    fn default() -> Self {
        Self {
            fix_thin: false,
            threads: None,
            skip_post_index_verify: false,
            merged_config: None,
            environment: crate::environment::Environment::empty(),
        }
    }
}

impl IngestPackOptions {
    fn index_parallelism(&self, odb: &Odb) -> Result<Parallelism> {
        if let Some(n) = self.threads {
            return Ok(Parallelism::resolve(Some(n)));
        }
        if let Some(cfg) = &self.merged_config {
            return crate::parallel::ThreadPolicy::default()
                .try_resolve(crate::parallel::ParallelOp::IndexPack, cfg)
                .map(|o| o.parallelism())
                .map_err(Error::Config);
        }
        if let Some(git_dir) = odb.config_git_dir() {
            let cfg = crate::config::ConfigSet::load(&self.environment, Some(git_dir), true)?;
            return crate::parallel::ThreadPolicy::default()
                .try_resolve(crate::parallel::ParallelOp::IndexPack, &cfg)
                .map(|o| o.parallelism())
                .map_err(Error::Config);
        }
        Ok(Parallelism::resolve(None))
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
    ingest_received_pack_with_progress(pack, odb, opts, None)
}

/// Like [`ingest_received_pack`], reporting pack indexing progress on `progress` when set.
pub fn ingest_received_pack_with_progress(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
) -> Result<IngestedPack> {
    if pack.is_empty() {
        return Ok(IngestedPack {
            object_ids: HashSet::new(),
            pack_path: PathBuf::new(),
            object_counts: ObjectKindCounts::default(),
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
    install_pack_bytes_with_progress(pack, odb, opts, progress)
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
    ingest_received_pack_path_with_progress(pack_path, odb, opts, None)
}

/// Like [`ingest_received_pack_path`], reporting pack indexing progress on `progress` when set.
pub fn ingest_received_pack_path_with_progress(
    pack_path: PathBuf,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
) -> Result<IngestedPack> {
    if !pack_path.is_file() {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("received pack file missing: {}", pack_path.display()),
        )));
    }
    install_pack_path_with_progress(&pack_path, odb, opts, progress)
}

/// Write `pack` into `odb`'s `objects/pack/` directory with a v2 index.
///
/// Builds under temporary names and publishes the `.pack`/`.idx` pair atomically.
pub fn install_pack_bytes(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    install_pack_bytes_with_progress(pack, odb, opts, None)
}

/// Like [`install_pack_bytes`], reporting pack indexing progress on `progress` when set.
pub fn install_pack_bytes_with_progress(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
) -> Result<IngestedPack> {
    odb.require_files_primary("index_pack")?;
    let pack_dir = odb.objects_dir().join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    static INSTALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = INSTALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = pack_dir.join(format!("tmp_install_{}_{seq}", std::process::id()));
    std::fs::write(&tmp, &pack).map_err(Error::Io)?;
    match install_pack_path_with_progress(&tmp, odb, opts, progress) {
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
    install_pack_path_with_progress(pack_path, odb, opts, None)
}

/// Like [`install_pack_path`], reporting pack indexing progress on `progress` when set.
pub fn install_pack_path_with_progress(
    pack_path: &Path,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
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
        let parallelism = opts.index_parallelism(odb)?;
        record_resolved_ingest_threads(parallelism.threads().get());
        let sort_threads = parallelism.threads().get();
        let (records, trailer) = {
            let pack_bytes: &[u8] = indexed.deref();
            verify_trailer(odb.hash_algo(), pack_bytes).map_err(|e| {
                Error::CorruptObject(format!(
                    "received pack checksum mismatch (download may be incomplete): {e}"
                ))
            })?;
            let records = crate::pack_index_build::build_pack_index_records_with_progress(
                pack_bytes,
                odb,
                parallelism,
                !opts.skip_post_index_verify,
                progress,
            )?;
            let trailer = pack_bytes[pack_bytes.len() - hb..].to_vec();
            (records, trailer)
        };
        drop(indexed);
        let oids: HashSet<ObjectId> = records.iter().map(|r| r.oid).collect();
        let object_counts: ObjectKindCounts = records.iter().map(|r| r.kind).collect();
        let entries: Vec<(ObjectId, u64, u32)> = records
            .into_iter()
            .map(
                |PackIndexRecord {
                     oid, offset, crc32, ..
                 }| (oid, offset, crc32),
            )
            .collect();
        write_v2_pack_index_with_trailer(&stage_idx, &entries, &trailer, hb, Some(sort_threads))?;
        if !opts.skip_post_index_verify {
            verify_pack_and_collect(&stage_idx)?;
        }
        std::fs::rename(&stage_pack, &final_pack).map_err(Error::Io)?;
        std::fs::rename(&stage_idx, &idx_path).map_err(|e| {
            let _ = std::fs::remove_file(&final_pack);
            Error::Io(e)
        })?;
        let _ = std::fs::remove_dir(&stage);
        Ok(IngestedPack {
            object_ids: oids,
            pack_path: final_pack.clone(),
            object_counts,
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
    use crate::unpack_objects::pack_index_records_with_threads;
    use std::process::Command;

    fn git_index_pack(pack: &[u8]) -> (tempfile::TempDir, Vec<u8>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let pack_path = dir.path().join("in.pack");
        std::fs::write(&pack_path, pack).expect("write");
        let out = Command::new("git") // hygiene: git index-pack cross-check in cfg(test) module
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

    #[test]
    fn object_kind_counts_tally_and_total() {
        let counts: ObjectKindCounts = [
            ObjectKind::Commit,
            ObjectKind::Blob,
            ObjectKind::Blob,
            ObjectKind::Tree,
            ObjectKind::Tag,
            ObjectKind::Commit,
        ]
        .into_iter()
        .collect();
        assert_eq!(
            counts,
            ObjectKindCounts {
                commits: 2,
                trees: 1,
                blobs: 2,
                tags: 1,
            }
        );
        assert_eq!(counts.total(), 6);
        assert_eq!(ObjectKindCounts::default().total(), 0);
    }

    #[test]
    fn ingest_counts_objects_by_resolved_kind() {
        use grit_test_support::objects::{HashAlgo, ObjectKind as PK, PackBuilder};
        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        pb.add_full(
            PK::Commit,
            b"tree 0000000000000000000000000000000000000000\n\nc\n",
        );
        pb.add_full(PK::Tree, b"");
        let base = b"blob base contents\n";
        let base_idx = pb.add_full(PK::Blob, base);
        let target = b"blob base contents, edited\n";
        let delta = crate::delta_encode::encode_lcp_delta(base, target).expect("delta");
        // A delta resolves to its base's type, so it counts as a blob.
        pb.add_ofs_delta(base_idx, &delta, delta.len());
        pb.add_full(
            PK::Tag,
            b"object 0000000000000000000000000000000000000000\n",
        );
        let pack = pb.build().bytes;

        let tmp = tempfile::tempdir().expect("tempdir");
        let objects = tmp.path().join("objects");
        std::fs::create_dir_all(objects.join("pack")).expect("pack dir");
        let odb = Odb::new(&objects);
        let ingested =
            ingest_received_pack(pack, &odb, &IngestPackOptions::default()).expect("ingest");
        assert_eq!(
            ingested.object_counts,
            ObjectKindCounts {
                commits: 1,
                trees: 1,
                blobs: 2,
                tags: 1,
            }
        );
        assert_eq!(
            ingested.object_counts.total(),
            u64::try_from(ingested.object_ids.len()).expect("count")
        );
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
        let path = env::var("GRIT_CARGO_PACK").expect("GRIT_CARGO_PACK"); // hygiene: manual RSS harness env in cfg(test)
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
        let out = Command::new("/usr/bin/git") // hygiene: git fixture helper in cfg(test) module
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

        let tip_out = Command::new("/usr/bin/git") // hygiene: git fixture cross-check in cfg(test) module
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
            None,
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
        let out = Command::new("/usr/bin/git") // hygiene: git index-pack strict cross-check in cfg(test) module
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

    fn pack_with_whole_objects_and_ofs_deltas() -> (Vec<u8>, usize, usize) {
        use grit_test_support::objects::{HashAlgo as FixtureAlgo, ObjectKind as PK, PackBuilder};

        let mut pb = PackBuilder::new(FixtureAlgo::Sha1);
        for i in 0..6 {
            pb.add_full(PK::Blob, format!("standalone-{i}\n").as_bytes());
        }
        let mut base_indices = Vec::new();
        for i in 0..4 {
            let data = format!("delta-base-{i}\n");
            base_indices.push(pb.add_full(PK::Blob, data.as_bytes()));
        }
        for (i, base_idx) in base_indices.into_iter().enumerate() {
            let base = format!("delta-base-{i}\n");
            let target = format!("delta-base-{i}-extended\n");
            let delta = crate::delta_encode::encode_lcp_delta(base.as_bytes(), target.as_bytes())
                .expect("delta");
            pb.add_ofs_delta(base_idx, &delta, delta.len());
        }
        let pack = pb.build().bytes;
        let nr_objects = 6 + 4 + 4;
        let nr_deltas = 4;
        (pack, nr_objects, nr_deltas)
    }

    #[derive(Default)]
    struct IndexPackProgressCapture {
        active: Option<crate::progress::ProgressPhase>,
        phase_totals: Vec<(crate::progress::ProgressPhase, Option<u64>)>,
        phase_max: std::collections::HashMap<crate::progress::ProgressPhase, u64>,
        set_samples: Vec<u64>,
    }

    impl crate::progress::ProgressSink for IndexPackProgressCapture {
        fn start(&mut self, phase: crate::progress::ProgressPhase, total: Option<u64>) {
            self.active = Some(phase);
            self.phase_totals.push((phase, total));
        }

        fn set(&mut self, current: u64) {
            self.set_samples.push(current);
            if let Some(phase) = self.active {
                let entry = self.phase_max.entry(phase).or_insert(0);
                *entry = (*entry).max(current);
            }
        }

        fn finish(&mut self) {
            self.active = None;
        }
    }

    #[test]
    fn progress_reports_object_and_delta_totals() {
        use crate::hash::Parallelism;
        use crate::unpack_objects::pack_index_records_with_threads_with_progress;

        let (pack, nr_objects, nr_deltas) = pack_with_whole_objects_and_ofs_deltas();
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());

        for threads in [1_usize, 4] {
            let mut cap = IndexPackProgressCapture::default();
            let records = pack_index_records_with_threads_with_progress(
                &pack,
                &odb,
                Parallelism::resolve(Some(threads)),
                Some(&mut cap),
            )
            .unwrap_or_else(|e| panic!("threads={threads}: {e}"));
            assert_eq!(records.len(), nr_objects, "threads={threads}");
            assert_eq!(
                cap.phase_max
                    .get(&crate::progress::ProgressPhase::IndexingObjects)
                    .copied(),
                Some(nr_objects as u64),
                "threads={threads}"
            );
            assert_eq!(
                cap.phase_max
                    .get(&crate::progress::ProgressPhase::ResolvingDeltas)
                    .copied(),
                Some(nr_deltas as u64),
                "threads={threads}"
            );
            let indexing_total = cap
                .phase_totals
                .iter()
                .find(|(p, _)| *p == crate::progress::ProgressPhase::IndexingObjects)
                .and_then(|(_, t)| *t);
            assert_eq!(indexing_total, Some(nr_objects as u64), "threads={threads}");
            let delta_total = cap
                .phase_totals
                .iter()
                .find(|(p, _)| *p == crate::progress::ProgressPhase::ResolvingDeltas)
                .and_then(|(_, t)| *t);
            assert_eq!(delta_total, Some(nr_deltas as u64), "threads={threads}");
        }
    }

    #[derive(Default)]
    struct MonotonicPhaseCapture {
        active: Option<crate::progress::ProgressPhase>,
        sets_by_phase: std::collections::HashMap<crate::progress::ProgressPhase, Vec<u64>>,
    }

    impl crate::progress::ProgressSink for MonotonicPhaseCapture {
        fn start(&mut self, phase: crate::progress::ProgressPhase, _total: Option<u64>) {
            self.active = Some(phase);
            self.sets_by_phase.entry(phase).or_default();
        }

        fn set(&mut self, current: u64) {
            if let Some(phase) = self.active {
                self.sets_by_phase.entry(phase).or_default().push(current);
            }
        }

        fn finish(&mut self) {
            self.active = None;
        }
    }

    #[test]
    fn progress_counts_monotonic() {
        use crate::hash::Parallelism;
        use crate::unpack_objects::pack_index_records_with_threads_with_progress;

        let (pack, _, _) = pack_with_whole_objects_and_ofs_deltas();
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let mut cap = MonotonicPhaseCapture::default();
        pack_index_records_with_threads_with_progress(
            &pack,
            &odb,
            Parallelism::resolve(Some(4)),
            Some(&mut cap),
        )
        .expect("index with progress");

        for (phase, samples) in &cap.sets_by_phase {
            let mut last = 0_u64;
            for &cur in samples {
                assert!(
                    cur >= last,
                    "phase {phase:?} regressed from {last} to {cur}: {samples:?}"
                );
                last = cur;
            }
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
