//! Install a received packfile into the object store (`index-pack` path).
//!
//! Fetch and clone keep the negotiated pack on disk (`.pack` + `.idx`) instead of
//! exploding every object into loose storage. Small receive-pack pushes may still
//! use [`crate::unpack_objects`] via [`crate::receive_pack::should_use_unpack_objects`].

use std::collections::HashSet;
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use tempfile::TempPath;

use crate::error::{Error, Result};
use crate::fs_publish::publish_no_clobber;
use crate::hash::{verify_trailer, Parallelism};
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::store::touch_pack_mtime;
use crate::odb::Odb;
use crate::pack::{verify_pack_and_collect, write_v2_pack_index_sorting};
use crate::pack_index_build::{build_mapped_pack_index, RESIDENT_PACK_BYTES};
use crate::pack_map::{PackData, ResidentBudget};
use crate::progress::{poll_cancel, Cancel, ProgressSink};
use crate::unpack_objects::{IngestedObjects, PackIndexRecord};

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

impl std::ops::AddAssign for ObjectKindCounts {
    fn add_assign(&mut self, other: Self) {
        self.commits += other.commits;
        self.trees += other.trees;
        self.blobs += other.blobs;
        self.tags += other.tags;
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

/// Result of ingesting a received pack into an object database.
#[derive(Debug, Clone, Default)]
pub struct IngestedPack {
    /// Object ids the pack delivered.
    pub object_ids: HashSet<ObjectId>,
    /// Absolute path to the published `pack-*.pack` file.
    ///
    /// `None` when nothing was published: the pack was empty, or the object database
    /// has a custom [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary) store and
    /// the objects were written into it one by one.
    pub pack_path: Option<PathBuf>,
    /// How many objects of each type the pack holds.
    pub object_counts: ObjectKindCounts,
    /// Size in bytes of the published pack (after completing a thin pack), or of the
    /// pack stream when the objects went to a custom primary store.
    pub pack_bytes: u64,
}

impl From<IngestedObjects> for IngestedPack {
    fn from(ingested: IngestedObjects) -> Self {
        Self {
            object_ids: ingested.object_ids,
            pack_path: None,
            object_counts: ingested.object_counts,
            pack_bytes: ingested.pack_bytes,
        }
    }
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
    /// When true, write an empty sibling `.promisor` file next to the installed pack.
    pub mark_promisor_pack: bool,
    /// Rows for a pack built in-process (`oid`, pack offset, per-object CRC32). When set, the
    /// pack body is not rescanned to rebuild the index (local repack/gc only); together with
    /// [`Self::skip_post_index_verify`] the pack's trailer is trusted as written too, so the
    /// body is not read at all.
    pub trusted_index_rows: Option<Vec<(ObjectId, u64, u32)>>,
}

impl Default for IngestPackOptions {
    fn default() -> Self {
        Self {
            fix_thin: false,
            threads: None,
            skip_post_index_verify: false,
            merged_config: None,
            environment: crate::environment::Environment::empty(),
            mark_promisor_pack: false,
            trusted_index_rows: None,
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
///
/// When `odb` has a custom [`OdbBuilder::primary`](crate::odb::OdbBuilder::primary)
/// store, the pack is not installed: its objects are resolved and written into that
/// store (see [`ingest_pack_into`](crate::unpack_objects::ingest_pack_into)), thin-pack
/// bases are read through `odb`, and [`IngestedPack::pack_path`] is `None`. Only
/// the pack checksum is verified then; [`IngestPackOptions`] fields that describe the
/// installed `.pack`/`.idx` pair (threads, promisor marker, trusted rows) do not apply.
///
/// # Errors
///
/// [`Error::CorruptObject`] when the bytes are not a complete pack, plus any
/// indexing, delta-resolution, or object-store failure.
pub fn ingest_received_pack(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    ingest_received_pack_with_progress(pack, odb, opts, None, None)
}

/// Like [`ingest_received_pack`], reporting pack indexing progress on `progress` when set.
pub fn ingest_received_pack_with_progress(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<IngestedPack> {
    if pack.is_empty() {
        return Ok(IngestedPack::default());
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
    if !odb.uses_files_primary() {
        return ingest_into_custom_primary(&mut pack.as_slice(), odb, progress, cancel);
    }
    install_pack_bytes_with_progress(pack, odb, opts, progress, cancel)
}

/// Resolve every object of the pack on `reader` into `odb`'s custom primary store.
fn ingest_into_custom_primary(
    reader: &mut dyn std::io::Read,
    odb: &Odb,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<IngestedPack> {
    crate::unpack_objects::ingest_pack_into_odb(
        reader,
        odb,
        &crate::unpack_objects::UnpackOptions::default(),
        progress,
        cancel,
    )
    .map(IngestedPack::from)
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
    ingest_received_pack_path_with_progress(pack_path, odb, opts, None, None)
}

fn wait_for_received_pack_path(path: PathBuf) -> Result<PathBuf> {
    use std::io::ErrorKind;
    use std::thread;
    use std::time::Duration;

    const MAX_ATTEMPTS: u32 = if cfg!(windows) { 12 } else { 4 };
    for attempt in 0..MAX_ATTEMPTS {
        if attempt > 0 {
            thread::sleep(Duration::from_millis(25_u64.saturating_mul(attempt as u64)));
        }
        match std::fs::metadata(&path) {
            Ok(meta) if meta.is_file() => return Ok(path),
            Ok(_) => break,
            Err(e) if e.kind() == ErrorKind::NotFound => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::ReceivedPackTempMissing { path })
}

/// Like [`ingest_received_pack_path`], reporting pack indexing progress on `progress` when set.
///
/// With a custom primary store the objects are written into it as described on
/// [`ingest_received_pack`], and the file at `pack_path` is removed afterwards
/// (whether or not ingestion succeeded), just as a files-backed ingest moves it.
pub fn ingest_received_pack_path_with_progress(
    pack_path: PathBuf,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<IngestedPack> {
    crate::trace::trace_span!("grit.pack.index");
    let pack_path = wait_for_received_pack_path(pack_path)?;
    if !odb.uses_files_primary() {
        let ingested = std::fs::File::open(&pack_path)
            .map_err(Error::Io)
            .and_then(|file| {
                let mut reader = std::io::BufReader::with_capacity(1 << 20, file);
                ingest_into_custom_primary(&mut reader, odb, progress, cancel)
            });
        let _ = std::fs::remove_file(&pack_path);
        return ingested;
    }
    install_pack_path_with_progress(&pack_path, odb, opts, progress, cancel)
}

/// Write `pack` into `odb`'s `objects/pack/` directory with a v2 index.
///
/// Builds under temporary names and publishes the `.pack`/`.idx` pair atomically.
pub fn install_pack_bytes(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    install_pack_bytes_with_progress(pack, odb, opts, None, None)
}

/// Like [`install_pack_bytes`], reporting pack indexing progress on `progress` when set.
pub fn install_pack_bytes_with_progress(
    pack: Vec<u8>,
    odb: &Odb,
    opts: &IngestPackOptions,
    progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<IngestedPack> {
    odb.require_files_primary("index_pack")?;
    let pack_dir = odb.objects_dir().join("pack");
    std::fs::create_dir_all(&pack_dir).map_err(Error::Io)?;
    let tmp = pack_dir.join(format!("tmp_install_{}", unique_install_suffix()));
    std::fs::write(&tmp, &pack).map_err(Error::Io)?;
    match install_pack_path_with_progress(&tmp, odb, opts, progress, cancel) {
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
///
/// Several threads or processes may install the same pack into one repository at
/// once (parallel fetches): each stages under its own name, and a `.pack`/`.idx`
/// already published under the same content-addressed name is kept, freshened, and
/// reported as success rather than replaced. A failed or cancelled install removes
/// only its private staging files, never a published `.pack` or `.idx`, because a
/// concurrent installer may already have reported success relying on it.
pub fn install_pack_path(
    pack_path: &Path,
    odb: &Odb,
    opts: &IngestPackOptions,
) -> Result<IngestedPack> {
    install_pack_path_with_progress(pack_path, odb, opts, None, None)
}

/// Like [`install_pack_path`], reporting pack indexing progress on `progress` when set.
///
/// When `cancel` is set, the coordinator polls it at phase boundaries; worker threads
/// during parallel hashing only observe a shared atomic snapshot (see library guide
/// [Parallelism](https://grit-scm.com/docs/library/parallelism)).
pub fn install_pack_path_with_progress(
    pack_path: &Path,
    odb: &Odb,
    opts: &IngestPackOptions,
    mut progress: Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<IngestedPack> {
    odb.require_files_primary("index_pack")?;
    let owned = pack_path.to_path_buf();
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

    // Unique per install: processes receiving the same pack must not share a stage.
    let stage = pack_dir.join(format!(".{stem}-install-{}", unique_install_suffix()));
    let stage_pack = stage.join(format!("{stem}.pack"));
    let stage_idx = stage.join(format!("{stem}.idx"));

    let install_result = (|| -> Result<IngestedPack> {
        poll_ingest_cancel(&mut progress, cancel)?;
        std::fs::create_dir_all(&stage).map_err(Error::Io)?;
        if std::fs::rename(&owned, &stage_pack).is_err() {
            std::fs::copy(&owned, &stage_pack).map_err(Error::Io)?;
            let _ = std::fs::remove_file(&owned);
        }
        poll_ingest_cancel(&mut progress, cancel)?;
        let parallelism = opts.index_parallelism(odb)?;
        record_resolved_ingest_threads(parallelism.threads().get());
        let sort_threads = parallelism.threads().get();
        // A pack built in-process with trusted rows and its trailer checked at
        // write time is installed without reading its body again.
        let trusted_as_written = opts
            .trusted_index_rows
            .as_deref()
            .filter(|_| opts.skip_post_index_verify);
        let mut trailer = trailer;
        let (mut entries, object_counts) = if let Some(rows) = trusted_as_written {
            if rows.len()
                != u32::from_be_bytes([header[8], header[9], header[10], header[11]]) as usize
            {
                return Err(Error::CorruptObject(
                    "trusted index row count does not match pack header".to_owned(),
                ));
            }
            (rows.to_vec(), ObjectKindCounts::default())
        } else {
            let indexed = PackData::open(&stage_pack)?;
            let resident = ResidentBudget::new(&indexed, RESIDENT_PACK_BYTES);
            let pack_bytes: &[u8] = indexed.deref();
            let body_len = pack_bytes.len() - hb;
            if resident.checksum(odb.hash_algo(), &pack_bytes[..body_len]) != pack_bytes[body_len..]
            {
                return Err(Error::CorruptObject(
                    "received pack checksum mismatch (download may be incomplete)".to_owned(),
                ));
            }
            poll_ingest_cancel(&mut progress, cancel)?;
            trailer = pack_bytes[body_len..].to_vec();
            if let Some(rows) = &opts.trusted_index_rows {
                if rows.len()
                    != u32::from_be_bytes([
                        pack_bytes[8],
                        pack_bytes[9],
                        pack_bytes[10],
                        pack_bytes[11],
                    ]) as usize
                {
                    return Err(Error::CorruptObject(
                        "trusted index row count does not match pack header".to_owned(),
                    ));
                }
                drop(indexed);
                (rows.clone(), ObjectKindCounts::default())
            } else {
                let file = std::fs::File::open(&stage_pack).map_err(Error::Io)?;
                let build = build_mapped_pack_index(
                    &indexed,
                    &file,
                    odb,
                    parallelism,
                    false,
                    progress.as_mut().map(|p| p as &mut dyn ProgressSink),
                    cancel,
                )?;
                drop(file);
                drop(indexed);
                let mut records = build.records;
                if opts.fix_thin && !build.thin_bases.is_empty() {
                    poll_ingest_cancel(&mut progress, cancel)?;
                    trailer = append_thin_bases(&stage_pack, odb, &build.thin_bases, &mut records)?;
                }
                let object_counts: ObjectKindCounts = records.iter().map(|r| r.kind).collect();
                let entries: Vec<(ObjectId, u64, u32)> = records
                    .into_iter()
                    .map(
                        |PackIndexRecord {
                             oid, offset, crc32, ..
                         }| (oid, offset, crc32),
                    )
                    .collect();
                (entries, object_counts)
            }
        };
        poll_ingest_cancel(&mut progress, cancel)?;
        write_v2_pack_index_sorting(&stage_idx, &mut entries, &trailer, hb, Some(sort_threads))?;
        // Rebuild the id set from the index just written, so the entries and the set are
        // never held at the same time.
        drop(entries);
        let index = crate::pack::read_pack_index_no_verify(&stage_idx)?;
        let mut oids: HashSet<ObjectId> = HashSet::with_capacity(index.len());
        for i in 0..index.len() {
            oids.insert(ObjectId::from_bytes(index.oid_at(i))?);
        }
        drop(index);
        poll_ingest_cancel(&mut progress, cancel)?;
        if !opts.skip_post_index_verify {
            verify_pack_and_collect(&stage_idx)?;
        }
        poll_ingest_cancel(&mut progress, cancel)?;
        let stem = format!("pack-{}", ObjectId::from_bytes(&trailer)?.to_hex());
        let final_pack = pack_dir.join(format!("{stem}.pack"));
        let idx_path = pack_dir.join(format!("{stem}.idx"));
        // Names are content-addressed: an existing `.pack`/`.idx` holds these bytes, and
        // a concurrent installer may already rely on a file this install published, so
        // published files are never rolled back. The pack goes first so a reader never
        // sees an index without its pack, and there is no cancellation point between
        // the two so a cancelled install does not strand a pack without its index.
        let publish = |staged: &Path, final_path: &Path| -> Result<bool> {
            let staged = TempPath::try_from_path(staged).map_err(Error::Io)?;
            publish_no_clobber(staged, final_path).map_err(Error::Io)
        };
        if !publish(&stage_pack, &final_pack)? {
            touch_pack_mtime(&final_pack);
        }
        publish(&stage_idx, &idx_path)?;
        if opts.mark_promisor_pack {
            std::fs::write(final_pack.with_extension("promisor"), b"").map_err(Error::Io)?;
        }
        let _ = std::fs::remove_dir(&stage);
        let pack_bytes = std::fs::metadata(&final_pack).map_err(Error::Io)?.len();
        Ok(IngestedPack {
            object_ids: oids,
            pack_path: Some(final_pack.clone()),
            object_counts,
            pack_bytes,
        })
    })();

    if install_result.is_err() {
        cleanup_install_stage(&stage);
        let _ = std::fs::remove_file(&owned);
    }
    let ingested = install_result?;
    odb.invalidate_packs();
    Ok(ingested)
}

/// Complete a thin pack in place (`index-pack --fix-thin`): append each of `bases` from
/// `odb` as a whole object, bump the header's object count and rewrite the trailer.
///
/// Objects are written straight to the file and the new checksum is computed by
/// streaming the file back, so the pack is never held in memory. A record for each
/// appended object is pushed onto `records`.
///
/// Returns the new pack trailer.
///
/// # Errors
///
/// Returns [`Error::Io`] on file errors, an object-database error when a base cannot be
/// read, and [`Error::CorruptObject`] when the object count would overflow.
fn append_thin_bases(
    pack_path: &Path,
    odb: &Odb,
    bases: &[ObjectId],
    records: &mut Vec<PackIndexRecord>,
) -> Result<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom, Write};

    let algo = odb.hash_algo();
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(pack_path)
        .map_err(Error::Io)?;
    let mut header = [0u8; 12];
    file.read_exact(&mut header).map_err(Error::Io)?;
    let added = u32::try_from(bases.len()).ok();
    let count = added
        .and_then(|added| {
            u32::from_be_bytes([header[8], header[9], header[10], header[11]]).checked_add(added)
        })
        .ok_or_else(|| Error::CorruptObject("thin pack fix: object count overflow".to_owned()))?;
    header[8..12].copy_from_slice(&count.to_be_bytes());

    let len = file.metadata().map_err(Error::Io)?.len();
    let mut offset = len - algo.len() as u64;
    file.seek(SeekFrom::Start(offset)).map_err(Error::Io)?;
    let mut object = Vec::new();
    for oid in bases {
        object.clear();
        let kind = crate::pack_objects::append_whole_object_from_odb(&mut object, odb, *oid)?;
        file.write_all(&object).map_err(Error::Io)?;
        records.push(PackIndexRecord {
            oid: *oid,
            offset,
            crc32: crc32fast::hash(&object),
            kind,
        });
        offset += object.len() as u64;
    }
    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    file.write_all(&header).map_err(Error::Io)?;

    file.seek(SeekFrom::Start(0)).map_err(Error::Io)?;
    let mut hasher = algo.checksum_hasher();
    let mut buf = vec![0u8; 1024 * 1024];
    let mut remaining = offset;
    while remaining > 0 {
        let want = usize::try_from(remaining).map_or(buf.len(), |r| r.min(buf.len()));
        file.read_exact(&mut buf[..want]).map_err(Error::Io)?;
        hasher.update(&buf[..want]);
        remaining -= want as u64;
    }
    let trailer = hasher.finalize();
    file.write_all(&trailer).map_err(Error::Io)?;
    Ok(trailer)
}

fn poll_ingest_cancel(
    progress: &mut Option<&mut dyn ProgressSink>,
    cancel: Option<&dyn Cancel>,
) -> Result<()> {
    let Some(c) = cancel else {
        return Ok(());
    };
    if let Some(p) = progress.as_mut() {
        poll_cancel(p, c)
    } else if c.is_cancelled() {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

/// `<pid>_<seq>`: unique among all installs running on this machine.
fn unique_install_suffix() -> String {
    static INSTALL_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = INSTALL_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{}_{seq}", std::process::id())
}

fn cleanup_install_stage(stage: &Path) {
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
    fn install_pack_cancelled_during_indexing_cleans_stage() {
        use crate::progress::{FlagCancel, ProgressPhase, ProgressSink, ProgressUnit};
        use grit_test_support::objects::{HashAlgo, ObjectKind as PK, PackBuilder};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct CancelAtIndexing {
            flag: FlagCancel,
            saw_indexing: Arc<AtomicBool>,
        }

        impl ProgressSink for CancelAtIndexing {
            fn start(&mut self, phase: ProgressPhase, total: Option<u64>) {
                if phase == ProgressPhase::IndexingObjects {
                    self.saw_indexing.store(true, Ordering::Relaxed);
                    self.flag.cancel();
                }
                let _ = total;
            }
            fn start_with_unit(
                &mut self,
                phase: ProgressPhase,
                unit: ProgressUnit,
                total: Option<u64>,
            ) {
                self.start(phase, total);
                let _ = unit;
            }
        }

        let mut pb = PackBuilder::new(HashAlgo::Sha1);
        for i in 0..400 {
            pb.add_full(PK::Blob, format!("blob-{i}\n").as_bytes());
        }
        let pack = pb.build().bytes;
        let tmp = tempfile::tempdir().expect("tempdir");
        let pack_path = tmp.path().join("incoming.pack");
        std::fs::write(&pack_path, &pack).expect("write pack");
        let odb = Odb::new(tmp.path());
        let flag = FlagCancel::new();
        let saw = Arc::new(AtomicBool::new(false));
        let mut progress = CancelAtIndexing {
            flag: flag.clone(),
            saw_indexing: Arc::clone(&saw),
        };
        let err = install_pack_path_with_progress(
            &pack_path,
            &odb,
            &IngestPackOptions {
                threads: Some(4),
                ..Default::default()
            },
            Some(&mut progress),
            Some(&flag),
        )
        .expect_err("cancel during index-pack");
        assert!(matches!(err, Error::Cancelled), "got {err:?}");
        assert!(saw.load(Ordering::Relaxed), "expected indexing phase");

        let pack_dir = tmp.path().join("pack");
        if pack_dir.exists() {
            for entry in pack_dir_entries(&pack_dir) {
                let name = entry
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                assert!(
                    !name.ends_with(".pack") && !name.ends_with(".idx"),
                    "leftover install artifact: {}",
                    entry.display()
                );
                assert!(
                    !entry.is_dir() || !name.contains("-install"),
                    "leftover stage dir: {}",
                    entry.display()
                );
            }
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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

        let oid = odb.hash(ObjectKind::Blob, blob).expect("hash");
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

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
        crate::pack::write_v2_pack_index_with_trailer(
            &idx_path,
            &[(fake_oid, huge_off, 0x1234_5678)],
            &pack[pack.len() - 20..],
            20,
            None,
        )
        .expect("write idx");
        let idx = read_pack_index(&idx_path).expect("read idx");
        assert_eq!(idx.len(), 1);
        assert_eq!(idx.offset_at(0).expect("off"), huge_off);
    }

    fn pack_with_junk_before_trailer(valid: &[u8], junk: &[u8]) -> Vec<u8> {
        use crate::objects::HashAlgo;

        let algo = HashAlgo::Sha1;
        let hb = algo.len();
        let body = &valid[..valid.len().checked_sub(hb).expect("pack trailer")];
        let mut body_with_junk = body.to_vec();
        body_with_junk.extend_from_slice(junk);
        let trailer = algo.checksum_digest(&body_with_junk);
        body_with_junk.extend_from_slice(&trailer);
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

    fn git_in(dir: &Path, args: &[&str]) -> Vec<u8> {
        let out = Command::new("git") // hygiene: git fixture/fsck cross-check in cfg(test) module
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out.stdout
    }

    /// A pack holding one commit with its tree and blob, plus the commit id.
    fn commit_pack() -> (Vec<u8>, String) {
        use std::io::Write;
        let src = tempfile::tempdir().expect("src");
        git_in(src.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(src.path().join("f"), b"shared pack\n").expect("write");
        git_in(src.path(), &["add", "f"]);
        git_in(src.path(), &["commit", "-qm", "c"]);
        let tip = String::from_utf8(git_in(src.path(), &["rev-parse", "HEAD"])).expect("hex");
        let tip = tip.trim().to_owned();
        let mut child = Command::new("git") // hygiene: git pack fixture in cfg(test) module
            .current_dir(src.path())
            .args(["pack-objects", "--revs", "--stdout", "-q"])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("pack-objects");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(format!("{tip}\n").as_bytes())
            .expect("revs");
        let out = child.wait_with_output().expect("pack-objects output");
        assert!(out.status.success());
        (out.stdout, tip)
    }

    fn published_files(pack_dir: &Path, ext: &str) -> usize {
        pack_dir_entries(pack_dir)
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == ext))
            .count()
    }

    fn assert_repo_fsck_clean(repo: &Path, tip: &str) {
        git_in(repo, &["update-ref", "refs/heads/main", tip]);
        git_in(repo, &["fsck", "--strict"]);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn cancel_once_pack_published_never_strands_a_peer_install() {
        use std::cell::Cell;

        /// Cancels the install as soon as its `.pack` is published, after running a
        /// complete peer install of the same pack that relies on that file.
        struct PeerThenCancel<'a> {
            pack_dir: PathBuf,
            objects: PathBuf,
            pack: &'a [u8],
            peer_ran: Cell<bool>,
        }

        impl Cancel for PeerThenCancel<'_> {
            fn is_cancelled(&self) -> bool {
                if self.peer_ran.get() {
                    return true;
                }
                if published_files(&self.pack_dir, "pack") == 0 {
                    return false;
                }
                self.peer_ran.set(true);
                let peer_odb = Odb::new(&self.objects);
                install_pack_bytes(self.pack.to_vec(), &peer_odb, &IngestPackOptions::default())
                    .expect("peer install succeeds");
                true
            }
        }

        let (pack, tip) = commit_pack();
        let repo = tempfile::tempdir().expect("repo");
        git_in(repo.path(), &["init", "-q", "-b", "main"]);
        let objects = repo.path().join(".git/objects");
        let pack_dir = objects.join("pack");
        let cancel = PeerThenCancel {
            pack_dir: pack_dir.clone(),
            objects: objects.clone(),
            pack: &pack,
            peer_ran: Cell::new(false),
        };
        let odb = Odb::new(&objects);
        let result = install_pack_bytes_with_progress(
            pack.clone(),
            &odb,
            &IngestPackOptions::default(),
            None,
            Some(&cancel),
        );
        match result {
            Ok(_) => {}
            Err(Error::Cancelled) => assert!(cancel.peer_ran.get()),
            Err(e) => panic!("unexpected install error: {e:?}"),
        }
        assert_eq!(published_files(&pack_dir, "pack"), 1);
        assert_eq!(published_files(&pack_dir, "idx"), 1);
        assert_repo_fsck_clean(repo.path(), &tip);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn error_after_publish_keeps_published_pack_and_index() {
        let (pack, tip) = commit_pack();
        let repo = tempfile::tempdir().expect("repo");
        git_in(repo.path(), &["init", "-q", "-b", "main"]);
        let objects = repo.path().join(".git/objects");
        let pack_dir = objects.join("pack");
        let trailer = ObjectId::from_bytes(&pack[pack.len() - 20..]).expect("trailer");
        // A directory where the `.promisor` marker goes makes the install fail only
        // after the `.pack`/`.idx` pair is published.
        std::fs::create_dir_all(pack_dir.join(format!("pack-{}.promisor", trailer.to_hex())))
            .expect("promisor obstacle");
        let odb = Odb::new(&objects);
        let err = install_pack_bytes(
            pack,
            &odb,
            &IngestPackOptions {
                mark_promisor_pack: true,
                ..Default::default()
            },
        )
        .expect_err("promisor write fails");
        assert!(matches!(err, Error::Io(_)), "got {err:?}");
        assert_eq!(published_files(&pack_dir, "pack"), 1);
        assert_eq!(published_files(&pack_dir, "idx"), 1);
        assert!(
            pack_dir_entries(&pack_dir).iter().all(|p| {
                let name = p.file_name().and_then(|n| n.to_str()).unwrap_or_default();
                !name.starts_with('.') && !name.starts_with("tmp_")
            }),
            "staging left behind"
        );
        assert_repo_fsck_clean(repo.path(), &tip);
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
                            .is_some_and(|n| n.starts_with('.') && n.contains("-install-"))),
                "must not publish partial .pack/.idx, got {entries:?}"
            );
        }
    }

    #[test]
    fn ingest_missing_received_pack_path_is_typed() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let odb = Odb::new(tmp.path());
        let missing = tmp.path().join("no-such-pack");
        let err = ingest_received_pack_path(missing.clone(), &odb, &IngestPackOptions::default())
            .expect_err("missing spool");
        assert!(
            matches!(err, Error::ReceivedPackTempMissing { .. }),
            "expected typed missing-pack error, got {err:?}"
        );
    }
}
