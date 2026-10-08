//! Loose object database: reading and writing zlib-compressed Git objects.
//!
//! Git stores objects as files under `<git-dir>/objects/<xx>/<38-hex-chars>`,
//! where the path is derived from the SHA-1 digest. Each file is a zlib-
//! compressed byte sequence whose decompressed form is:
//!
//! ```text
//! "<type> <size>\0<data>"
//! ```
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use grit_lib::odb::Odb;
//!
//! let odb = Odb::new(Path::new(".git/objects"));
//! ```

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::hash;
use crate::midx::{midx_oid_listed_in_tip, try_read_object_via_midx};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::pack;
use crate::zlib_inflate::ZlibInflateScratch;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use flate2::Compression;

type MemOdbOverlay = Arc<Mutex<Option<std::collections::HashMap<ObjectId, (ObjectKind, Vec<u8>)>>>>;

/// Cached `info/alternates` chain for one [`Odb`] (generation bumps on invalidation/reload).
struct FileAlternatesCache {
    generation: u64,
    snapshot: Option<Arc<Vec<PathBuf>>>,
}

/// Options for [`Odb::write_with_options`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteOptions {
    /// When set, do not touch mtimes on existing objects (Git `WRITE_OBJECT_SILENT`).
    pub silent: bool,
    /// When set, skip the full `Odb::exists` scan (packs/alternates) and only
    /// test for a loose file in this repository's object directory before writing.
    ///
    /// Bulk staging of new worktree blobs uses this so each insert does not walk
    /// every pack on a miss.
    pub assume_loose_only_existence: bool,
    /// When set, skip existence probes and write when the loose path is absent
    /// (bulk add of all-new blobs).
    pub trust_new_loose: bool,
}

/// Decompress a zlib-wrapped loose object payload from an open file.
///
/// When the zlib wrapper advertises a preset dictionary (FDICT), `flate2` typically fails with a
/// generic corrupt-stream error; map that to `"needs dictionary"` so callers match Git's messages
/// (`t1006-cat-file` zlib-dictionary test).
fn read_zlib_loose_payload(file: fs::File) -> Result<Vec<u8>> {
    let mut hdr = [0u8; 2];
    let mut read_file = file;
    read_file.read_exact(&mut hdr).map_err(Error::Io)?;
    let cmf_flg = u16::from(hdr[0]) << 8 | u16::from(hdr[1]);
    let looks_like_zlib_header = cmf_flg != 0 && cmf_flg % 31 == 0;
    let preset_dictionary = looks_like_zlib_header && (hdr[1] & 0x20) != 0;
    let mut scratch = ZlibInflateScratch::default();
    scratch.decompress_loose_payload(&hdr, read_file, preset_dictionary)
}

/// True when `oid` is stored as a loose object or in a **non-promisor** local pack.
///
/// Non-promisor pack membership is checked before loose storage so packed objects
/// avoid a per-probe loose `stat` on the hot path. A non-promisor pack hit already
/// implies the object is materialized locally, including when a loose copy also exists.
fn exists_materialized_in_objects_dir(objects_dir: &Path, oid: &ObjectId) -> bool {
    if object_in_local_packs(objects_dir, oid) {
        return true;
    }
    if oid.loose_path_in(objects_dir).is_file() {
        return true;
    }
    if pack::reprepare_pack_directory_on_miss(objects_dir).ok() == Some(true) {
        return object_in_local_packs(objects_dir, oid);
    }
    false
}

fn object_in_local_packs(objects_dir: &Path, oid: &ObjectId) -> bool {
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return false;
    };
    for idx in &indexes {
        if idx.is_promisor {
            continue;
        }
        if idx.contains(oid) {
            return true;
        }
    }
    false
}

/// A loose-object database rooted at a given `objects/` directory.
#[derive(Clone)]
pub struct Odb {
    objects_dir: PathBuf,
    /// Work tree root for resolving relative alternate env paths.
    work_tree: Option<PathBuf>,
    /// Embedded submodule object stores registered for this read pass (Git `register_all_submodule_sources`).
    submodule_alternate_dirs: Arc<Mutex<Vec<PathBuf>>>,
    /// When set, used to read `core.multiPackIndex` (and related) for MIDX-backed object reads.
    config_git_dir: Option<PathBuf>,
    /// Cache for `core.multiPackIndex` — populated on first lookup.
    ///
    /// Reading this config requires loading the system/global/local config cascade and reparsing
    /// every file; the value cannot change for a process that has opened a single repository, so
    /// caching it here avoids re-loading the cascade for every object read.
    core_multi_pack_index_cache: Arc<OnceLock<bool>>,
    /// `info/alternates` chain resolved lazily; generation bumps on invalidation/reload.
    file_alternate_dirs_cache: Arc<RwLock<FileAlternatesCache>>,
    #[cfg(test)]
    exists_probe: Arc<AtomicUsize>,
    #[cfg(test)]
    pub(crate) hot_path_test_metrics: Arc<crate::hot_path_test_metrics::HotPathTestMetrics>,
    /// Explicit env alternates from [`Self::with_env_alternate_dirs`] (tests / callers); empty uses lazy env read.
    env_alternate_dirs: Vec<PathBuf>,
    /// Lazily parsed `GIT_ALTERNATE_OBJECT_DIRECTORIES` (first alternate lookup only).
    env_alternate_lazy: Arc<OnceLock<Arc<Vec<PathBuf>>>>,
    /// When `Some`, object writes are redirected into this in-memory overlay instead of being
    /// persisted to the loose store (Git's tmp-objdir). Reads consult the overlay first. This
    /// mirrors `git merge-tree --quiet`, which performs a full merge but must leave the object
    /// database untouched (no new loose objects).
    mem_overlay: MemOdbOverlay,
    /// The repository's object hash algorithm (`extensions.objectformat`),
    /// detected lazily from the config and cached. Determines the hash used
    /// when writing objects. Defaults to SHA-1 when no config is available.
    hash_algo_cache: Arc<OnceLock<HashAlgo>>,
    /// Zlib level for loose-object writes, resolved from config and cached.
    loose_zlib_cache: Arc<OnceLock<Compression>>,
    /// Shared with [`crate::repo::Repository`] so config is loaded once per open handle.
    shared_config_state: Option<crate::repo::RepositoryConfigSnapshot>,
    /// Pack files whose mtimes were already bumped for object freshening on this [`Odb`]
    /// (Git's `packed_git->freshened`: at most one `utimensat` per pack per process).
    freshened_packs: Arc<Mutex<HashSet<PathBuf>>>,
}

impl std::fmt::Debug for Odb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Odb")
            .field("objects_dir", &self.objects_dir)
            .field("work_tree", &self.work_tree)
            .field("submodule_alternate_dirs", &"<mutex>")
            .field("config_git_dir", &self.config_git_dir)
            .finish()
    }
}

/// Resolve the repository hash algorithm from `extensions.objectformat` in `git_dir`'s config.
///
/// Uses the same config parser as [`Odb::hash_algo`], including case-insensitive section and
/// key names. Defaults to [`HashAlgo::Sha1`] when the extension is absent or unreadable.
#[must_use]
pub fn hash_algo_for_git_dir(git_dir: &Path) -> HashAlgo {
    ConfigSet::load(Some(git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get("extensions.objectformat"))
        .and_then(|v| HashAlgo::from_name(&v))
        .unwrap_or(HashAlgo::Sha1)
}

/// Like [`hash_algo_for_git_dir`] but takes an `objects/` directory path.
#[must_use]
pub fn hash_algo_for_objects_dir(objects_dir: &Path) -> HashAlgo {
    objects_dir
        .parent()
        .map(hash_algo_for_git_dir)
        .unwrap_or(HashAlgo::Sha1)
}

impl Odb {
    /// Create an [`Odb`] pointing at the given `objects/` directory.
    ///
    /// The directory does not need to exist yet; it will be created on the
    /// first write operation. Does not read `GIT_ALTERNATE_OBJECT_DIRECTORIES`;
    /// use [`Self::env_alternate_dirs_from_var`] and [`Self::with_env_alternate_dirs`]
    /// (or open via [`crate::repo::Repository`], which resolves env alternates lazily on first use).
    #[must_use]
    pub fn new(objects_dir: &Path) -> Self {
        Self {
            objects_dir: objects_dir.to_path_buf(),
            work_tree: None,
            submodule_alternate_dirs: Arc::new(Mutex::new(Vec::new())),
            config_git_dir: None,
            core_multi_pack_index_cache: Arc::new(OnceLock::new()),
            file_alternate_dirs_cache: Arc::new(RwLock::new(FileAlternatesCache {
                generation: 0,
                snapshot: None,
            })),
            env_alternate_dirs: Vec::new(),
            env_alternate_lazy: Arc::new(OnceLock::new()),
            mem_overlay: Arc::new(Mutex::new(None)),
            hash_algo_cache: Arc::new(OnceLock::new()),
            loose_zlib_cache: Arc::new(OnceLock::new()),
            shared_config_state: None,
            freshened_packs: Arc::new(Mutex::new(HashSet::new())),
            #[cfg(test)]
            exists_probe: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            hot_path_test_metrics: crate::hot_path_test_metrics::HotPathTestMetrics::new(),
        }
    }

    /// Create an [`Odb`] with a work tree for resolving relative alternate paths.
    ///
    /// Does not read `GIT_ALTERNATE_OBJECT_DIRECTORIES`; pass env-derived paths with
    /// [`Self::with_env_alternate_dirs`] (see [`Self::env_alternate_dirs_from_var`]).
    #[must_use]
    pub fn with_work_tree(objects_dir: &Path, work_tree: &Path) -> Self {
        Self {
            objects_dir: objects_dir.to_path_buf(),
            work_tree: Some(work_tree.to_path_buf()),
            submodule_alternate_dirs: Arc::new(Mutex::new(Vec::new())),
            config_git_dir: None,
            core_multi_pack_index_cache: Arc::new(OnceLock::new()),
            file_alternate_dirs_cache: Arc::new(RwLock::new(FileAlternatesCache {
                generation: 0,
                snapshot: None,
            })),
            env_alternate_dirs: Vec::new(),
            env_alternate_lazy: Arc::new(OnceLock::new()),
            mem_overlay: Arc::new(Mutex::new(None)),
            hash_algo_cache: Arc::new(OnceLock::new()),
            loose_zlib_cache: Arc::new(OnceLock::new()),
            shared_config_state: None,
            freshened_packs: Arc::new(Mutex::new(HashSet::new())),
            #[cfg(test)]
            exists_probe: Arc::new(AtomicUsize::new(0)),
            #[cfg(test)]
            hot_path_test_metrics: crate::hot_path_test_metrics::HotPathTestMetrics::new(),
        }
    }

    /// Hot-path regression counters for this object database (unit tests only).
    #[cfg(test)]
    #[must_use]
    pub fn hot_path_test_metrics(&self) -> &crate::hot_path_test_metrics::HotPathTestMetrics {
        &self.hot_path_test_metrics
    }

    /// Share the repository's lazy config snapshot (see [`crate::repo::Repository::config`]).
    #[must_use]
    pub(crate) fn with_shared_config_state(
        mut self,
        state: crate::repo::RepositoryConfigSnapshot,
    ) -> Self {
        self.shared_config_state = Some(state);
        self
    }

    fn load_config_cascade(&self) -> Result<ConfigSet> {
        if let Some(state) = &self.shared_config_state {
            let git_dir = self
                .config_git_dir
                .as_deref()
                .or_else(|| self.objects_dir.parent());
            let cfg = crate::repo::ensure_shared_config_snapshot(state, git_dir)?;
            return Ok(cfg.as_ref().clone());
        }
        let git_dir = self
            .config_git_dir
            .as_deref()
            .or_else(|| self.objects_dir.parent());
        if let Some(git_dir) = git_dir {
            ConfigSet::load(Some(git_dir), true)
        } else {
            Ok(ConfigSet::new())
        }
    }

    fn env_alternate_dirs_snapshot(&self) -> Arc<Vec<PathBuf>> {
        if !self.env_alternate_dirs.is_empty() {
            return Arc::new(self.env_alternate_dirs.clone());
        }
        Arc::clone(
            self.env_alternate_lazy.get_or_init(|| {
                Arc::new(Self::env_alternate_dirs_from_var(self.work_tree.as_deref()))
            }),
        )
    }

    /// Parse `GIT_ALTERNATE_OBJECT_DIRECTORIES` once for [`Self::with_env_alternate_dirs`].
    ///
    /// Relative entries are resolved against `resolve_base` (typically the work tree root).
    #[must_use]
    pub fn env_alternate_dirs_from_var(resolve_base: Option<&Path>) -> Vec<PathBuf> {
        match std::env::var("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
            Ok(val) if !val.is_empty() => {
                let mut dirs = parse_alternate_env(&val);
                if let Some(base) = resolve_base {
                    for dir in &mut dirs {
                        if dir.is_relative() {
                            *dir = base.join(&dir);
                        }
                    }
                }
                dirs
            }
            _ => Vec::new(),
        }
    }

    /// Attach env-derived alternate object directories (see [`Self::env_alternate_dirs_from_var`]).
    #[must_use]
    pub fn with_env_alternate_dirs(mut self, dirs: Vec<PathBuf>) -> Self {
        self.env_alternate_dirs = dirs;
        self
    }

    /// Drop the cached `info/alternates` chain so the next lookup re-reads the file.
    pub fn invalidate_alternates_cache(&self) {
        if let Ok(mut guard) = self.file_alternate_dirs_cache.write() {
            guard.generation = guard.generation.wrapping_add(1);
            guard.snapshot = None;
        }
    }

    /// Reload `info/alternates` from disk into this [`Odb`]'s cache (used after external writes).
    ///
    /// # Errors
    ///
    /// Propagates failures from reading the alternates chain (I/O or malformed `info/alternates`).
    pub fn refresh_file_alternates_from_disk(&self) -> Result<()> {
        let snapshot = Arc::new(pack::read_alternates_recursive(&self.objects_dir)?);
        if let Ok(mut guard) = self.file_alternate_dirs_cache.write() {
            guard.generation = guard.generation.wrapping_add(1);
            guard.snapshot = Some(snapshot);
        }
        Ok(())
    }

    /// Append `alternate_objects_dir` to `objects/info/alternates` (deduped) and refresh the cache.
    ///
    /// # Errors
    ///
    /// [`Error::Io`] when the alternates file cannot be read or written.
    pub fn append_file_alternate(&self, alternate_objects_dir: &Path) -> Result<()> {
        Self::append_alternate_objects_line(&self.objects_dir, alternate_objects_dir)?;
        self.refresh_file_alternates_from_disk()?;
        Ok(())
    }

    /// Append one absolute `objects` directory line to `objects/info/alternates` (deduped).
    ///
    /// Does not update any in-memory cache; pair with [`Self::refresh_file_alternates_from_disk`]
    /// on the relevant [`Odb`] when the same process keeps using it.
    pub fn append_alternate_objects_line(
        objects_dir: &Path,
        alternate_objects_dir: &Path,
    ) -> Result<()> {
        let info = objects_dir.join("info");
        fs::create_dir_all(&info)?;
        let alt_path = info.join("alternates");
        let alt_abs = alternate_objects_dir
            .canonicalize()
            .unwrap_or_else(|_| alternate_objects_dir.to_path_buf());
        let line = format!("{}\n", alt_abs.display());
        let existing = match fs::read_to_string(&alt_path) {
            Ok(s) => s,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(Error::Io(e)),
        };
        if existing
            .lines()
            .any(|l| l.trim() == alt_abs.to_string_lossy())
        {
            return Ok(());
        }
        let mut out = existing;
        out.push_str(&line);
        fs::write(alt_path, out).map_err(Error::Io)
    }

    fn file_alternate_dirs_snapshot(&self) -> Arc<Vec<PathBuf>> {
        loop {
            let observed_gen = self
                .file_alternate_dirs_cache
                .read()
                .map(|g| g.generation)
                .unwrap_or(0);
            if let Ok(guard) = self.file_alternate_dirs_cache.read() {
                if guard.generation == observed_gen {
                    if let Some(snapshot) = &guard.snapshot {
                        return Arc::clone(snapshot);
                    }
                }
            }
            if let Ok(mut guard) = self.file_alternate_dirs_cache.write() {
                if guard.generation != observed_gen {
                    continue;
                }
                if let Some(snapshot) = &guard.snapshot {
                    return Arc::clone(snapshot);
                }
                let snapshot = Arc::new(
                    pack::read_alternates_recursive(&self.objects_dir).unwrap_or_default(),
                );
                guard.snapshot = Some(Arc::clone(&snapshot));
                return snapshot;
            }
            break;
        }
        Arc::new(Vec::new())
    }

    #[cfg(test)]
    fn exists_probe_count(&self) -> usize {
        self.exists_probe.load(Ordering::Relaxed)
    }

    #[cfg(test)]
    fn reset_exists_probe(&self) {
        self.exists_probe.store(0, Ordering::Relaxed);
    }

    /// Enable the in-memory write overlay (Git's tmp-objdir): subsequent [`Self::write`]/
    /// [`Self::write_local`] calls keep objects only in memory, and [`Self::read`] consults that
    /// overlay before the on-disk store. Used by `merge-tree --quiet` so a full merge can run
    /// without persisting any new loose objects.
    pub fn enable_mem_overlay(&self) {
        if let Ok(mut guard) = self.mem_overlay.lock() {
            *guard = Some(std::collections::HashMap::new());
        }
    }

    /// Disable the in-memory write overlay, discarding any objects accumulated in it.
    pub fn disable_mem_overlay(&self) {
        if let Ok(mut guard) = self.mem_overlay.lock() {
            *guard = None;
        }
    }

    /// Whether the in-memory write overlay is currently enabled.
    fn overlay_active(&self) -> bool {
        self.mem_overlay.lock().is_ok_and(|g| g.is_some())
    }

    /// Number of objects stored in the active mem overlay (tests only).
    #[cfg(test)]
    pub(crate) fn mem_overlay_len_for_tests(&self) -> Option<usize> {
        self.mem_overlay
            .lock()
            .ok()
            .and_then(|g| g.as_ref().map(std::collections::HashMap::len))
    }

    /// If the in-memory overlay is active, store `(kind, data)` under `oid` there and return
    /// `true`; otherwise return `false` so the caller falls through to the on-disk path.
    fn overlay_store(&self, oid: ObjectId, kind: ObjectKind, data: &[u8]) -> bool {
        if let Ok(mut guard) = self.mem_overlay.lock() {
            if let Some(map) = guard.as_mut() {
                map.entry(oid).or_insert_with(|| (kind, data.to_vec()));
                return true;
            }
        }
        false
    }

    /// Read `oid` from the in-memory overlay, if active and present.
    fn overlay_read(&self, oid: &ObjectId) -> Option<Object> {
        if let Ok(guard) = self.mem_overlay.lock() {
            if let Some(map) = guard.as_ref() {
                if let Some((kind, data)) = map.get(oid) {
                    return Some(Object {
                        kind: *kind,
                        data: data.clone(),
                    });
                }
            }
        }
        None
    }

    /// Register `<submodule-git-dir>/objects` for every stage-0 gitlink in `index` that has a
    /// checkout under `work_tree`, so reads can resolve submodule commits stored only in the
    /// nested repository (matches Git's `odb_add_submodule_source_by_path` / `register_all_submodule_sources`).
    pub fn register_submodule_object_directories_from_index(
        &self,
        work_tree: &Path,
        index: &crate::index::Index,
    ) {
        use crate::diff::submodule_embedded_git_dir;

        let Ok(mut dirs) = self.submodule_alternate_dirs.lock() else {
            return;
        };
        dirs.clear();
        for e in &index.entries {
            if e.stage() != 0 || e.mode != crate::index::MODE_GITLINK {
                continue;
            }
            let path_str = String::from_utf8_lossy(&e.path);
            let abs = work_tree.join(path_str.as_ref());
            let Some(sub_git) = submodule_embedded_git_dir(&abs) else {
                continue;
            };
            let objects = sub_git.join("objects");
            if !objects.is_dir() {
                continue;
            }
            let canon = objects.canonicalize().unwrap_or(objects);
            if !dirs.iter().any(|p| p == &canon) {
                dirs.push(canon);
            }
        }
    }

    /// Attach a git directory so [`Self::read`] can honor `core.multiPackIndex` when resolving packed objects.
    #[must_use]
    pub fn with_config_git_dir(mut self, git_dir: PathBuf) -> Self {
        self.config_git_dir = Some(git_dir);
        self
    }

    /// The repository's object hash algorithm, detected from
    /// `extensions.objectformat` and cached for the lifetime of this `Odb`.
    ///
    /// The git directory is taken from the attached `config_git_dir` when set,
    /// otherwise inferred as the parent of the `objects/` directory. Defaults
    /// to [`HashAlgo::Sha1`] when no config can be read.
    #[must_use]
    pub fn hash_algo(&self) -> HashAlgo {
        *self.hash_algo_cache.get_or_init(|| {
            if self.config_git_dir.is_none() && self.objects_dir.parent().is_none() {
                return HashAlgo::Sha1;
            }
            let cfg = self.load_config_cascade().unwrap_or_default();
            cfg.get("extensions.objectformat")
                .and_then(|v| HashAlgo::from_name(&v))
                .unwrap_or(HashAlgo::Sha1)
        })
    }

    fn resolve_loose_zlib_level(&self) -> Result<u32> {
        if self.config_git_dir.is_none() && self.objects_dir.parent().is_none() {
            return Ok(ConfigSet::LOOSE_OBJECTS_ZLIB_DEFAULT as u32);
        }
        let cfg = self.load_config_cascade()?;
        Ok(cfg.loose_objects_zlib_level()? as u32)
    }

    /// Zlib compression used for loose-object writes ([`ConfigSet::loose_objects_zlib_level`]),
    /// cached for the lifetime of this `Odb`.
    ///
    /// # Errors
    ///
    /// Returns errors from loading the config cascade or from invalid compression settings.
    pub(crate) fn loose_compression(&self) -> Result<Compression> {
        if let Some(cached) = self.loose_zlib_cache.get() {
            return Ok(*cached);
        }
        let level = self.resolve_loose_zlib_level()?;
        let compression = Compression::new(level);
        let _ = self.loose_zlib_cache.set(compression);
        Ok(compression)
    }

    fn core_multi_pack_index_enabled(&self) -> bool {
        // The system/global/local config cascade is expensive to load (the parser walks every
        // file from `/etc/gitconfig` through `.git/config`); calling it once per object lookup
        // dominated `status` runtime. Cache the result for the lifetime of this `Odb`.
        *self.core_multi_pack_index_cache.get_or_init(|| {
            if self.config_git_dir.is_none() {
                return false;
            }
            let cfg = self.load_config_cascade().unwrap_or_default();
            match cfg.get_bool("core.multiPackIndex") {
                Some(Ok(b)) => b,
                Some(Err(_)) => true,
                None => true,
            }
        })
    }

    /// Return the path to the `objects/` directory.
    #[must_use]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Return the attached git directory, if one was set with
    /// [`Self::with_config_git_dir`].
    ///
    /// Used by config- and ref-aware operations (delta islands, on-disk delta
    /// reuse) that need the repository root rather than just the object store.
    #[must_use]
    pub fn config_git_dir(&self) -> Option<&Path> {
        self.config_git_dir.as_deref()
    }

    /// Return the filesystem path for a given object ID.
    #[must_use]
    pub fn object_path(&self, oid: &ObjectId) -> PathBuf {
        oid.loose_path_in(&self.objects_dir)
    }

    /// Whether the object exists under this database directory only (loose or local packs).
    ///
    /// Unlike [`Self::exists`], this ignores `info/alternates` and
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES`. Used for partial-clone bookkeeping where
    /// objects reachable via alternates are still treated as "missing" until copied locally.
    ///
    /// Objects stored **only** in promisor packs (sibling `.promisor` marker next to the
    /// `.pack`) are treated as absent: Git considers them fetchable on demand, and
    /// `rev-list --missing=print` lists them until materialized as loose objects or a
    /// non-promisor pack.
    ///
    /// The empty tree object is treated as present without a loose file (matches Git).
    #[must_use]
    pub fn exists_local(&self, oid: &ObjectId) -> bool {
        if oid.is_canonical_empty_tree() {
            return true;
        }
        exists_materialized_in_objects_dir(&self.objects_dir, oid)
    }

    /// Check whether an object exists in the loose store or any pack file.
    #[must_use]
    pub fn exists(&self, oid: &ObjectId) -> bool {
        #[cfg(test)]
        self.exists_probe.fetch_add(1, Ordering::Relaxed);
        if oid.is_well_known_empty_tree() {
            return true;
        }
        if self.exists_in_dir(&self.objects_dir, oid) {
            return true;
        }
        let file_alts = self.file_alternate_dirs_snapshot();
        for alt_dir in file_alts.iter() {
            if self.exists_in_dir(alt_dir, oid) {
                return true;
            }
        }
        for alt_dir in self.env_alternate_dirs_snapshot().iter() {
            if self.exists_in_dir(alt_dir, oid) {
                return true;
            }
        }
        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if self.exists_in_dir(alt_dir, oid) {
                    return true;
                }
            }
        }
        false
    }

    /// Check whether an object exists in a specific objects directory.
    fn exists_in_dir(&self, objects_dir: &Path, oid: &ObjectId) -> bool {
        if object_in_local_packs(objects_dir, oid) {
            return true;
        }
        if oid.loose_path_in(objects_dir).is_file() {
            return true;
        }
        if pack::reprepare_pack_directory_on_miss(objects_dir).ok() == Some(true)
            && object_in_local_packs(objects_dir, oid)
        {
            return true;
        }
        if objects_dir == self.objects_dir.as_path()
            && self.config_git_dir.is_some()
            && self.core_multi_pack_index_enabled()
        {
            match midx_oid_listed_in_tip(objects_dir, oid, self.hash_algo()) {
                Ok(Some(true)) => return true,
                Ok(Some(false)) | Ok(None) => {}
                Err(_) => return false,
            }
        }
        false
    }

    /// Touch the loose object file or pack file containing `oid`, matching Git's
    /// `odb_freshen_object` (updates mtime so age-based prune keeps recently re-referenced objects).
    ///
    /// Returns `true` if an on-disk object was found and touched.
    #[must_use]
    pub fn freshen_object(&self, oid: &ObjectId) -> bool {
        if oid.is_well_known_empty_tree() {
            return false;
        }

        let loose = self.object_path(oid);
        if loose.is_file() {
            return self.touch_object_mtime(&loose).is_some();
        }

        if self.freshen_object_in_objects_dir(&self.objects_dir, oid) {
            return true;
        }

        let file_alts = self.file_alternate_dirs_snapshot();
        for alt_dir in file_alts.iter() {
            if self.freshen_object_in_objects_dir(alt_dir, oid) {
                return true;
            }
        }

        for alt_dir in self.env_alternate_dirs_snapshot().iter() {
            if self.freshen_object_in_objects_dir(alt_dir, oid) {
                return true;
            }
        }

        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if self.freshen_object_in_objects_dir(alt_dir, oid) {
                    return true;
                }
            }
        }

        false
    }

    fn freshen_object_in_objects_dir(&self, objects_dir: &Path, oid: &ObjectId) -> bool {
        let loose = objects_dir
            .join(oid.loose_prefix())
            .join(oid.loose_suffix());
        if loose.is_file() {
            return self.touch_object_mtime(&loose).is_some();
        }
        let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
            return false;
        };
        for idx in &indexes {
            if idx.contains(oid) {
                return self.freshen_pack_once(&idx.pack_path, idx.is_cruft);
            }
        }
        false
    }

    /// Touch `pack_path`'s mtime at most once for this [`Odb`], refreshing the pack cache signature.
    ///
    /// The path is recorded in [`Self::freshened_packs`] only after a successful touch, matching
    /// Git's `packed_git->freshened` assignment after `utime` succeeds.
    fn freshen_pack_once(&self, pack_path: &Path, is_cruft: bool) -> bool {
        let Ok(mut guard) = self.freshened_packs.lock() else {
            return false;
        };
        if guard.contains(pack_path) {
            return true;
        }
        if is_cruft {
            return false;
        }
        let Some(touched_at) = self.touch_object_mtime(pack_path) else {
            return false;
        };
        pack::refresh_pack_bytes_signature(pack_path, touched_at);
        guard.insert(pack_path.to_path_buf());
        true
    }

    /// When `oid` is already reachable, freshen it and return its id. Returns `None` when the
    /// object lives only in a pack and freshening failed so the caller can materialize a loose copy.
    fn try_freshen_existing(&self, oid: &ObjectId) -> Option<ObjectId> {
        if self.object_path(oid).is_file() {
            let _ = self.freshen_object(oid);
            return Some(*oid);
        }
        if !self.exists(oid) {
            return None;
        }
        if self.freshen_object(oid) {
            return Some(*oid);
        }
        None
    }

    fn try_freshen_existing_local(&self, oid: &ObjectId) -> Option<ObjectId> {
        if self.object_path(oid).is_file() {
            let _ = self.freshen_object(oid);
            return Some(*oid);
        }
        if exists_materialized_in_objects_dir(&self.objects_dir, oid) && self.freshen_object(oid) {
            return Some(*oid);
        }
        None
    }

    /// Read a loose object file at `path`, verifying the uncompressed payload hashes to `expected_oid`.
    ///
    /// Git stores loose objects under paths derived from the OID; if the file contents hash to a
    /// different id (for example after a mistaken `mv`), this returns [`Error::LooseHashMismatch`].
    ///
    /// # Errors
    ///
    /// - [`Error::Zlib`] — decompression failed.
    /// - [`Error::CorruptObject`] — header is malformed.
    /// - [`Error::LooseHashMismatch`] — payload OID does not match `expected_oid`.
    pub fn read_loose_verify_oid(path: &Path, expected_oid: &ObjectId) -> Result<Object> {
        let file = fs::File::open(path).map_err(Error::Io)?;
        let raw = read_zlib_loose_payload(file)?;
        let obj = parse_object_bytes_with_oid(&raw, expected_oid)?;
        // Verify against the expected OID using its own hash algorithm; a SHA-256
        // loose object must be re-hashed with SHA-256, not SHA-1.
        let computed = hash_object_data_with(expected_oid.algo(), obj.kind, &obj.data);
        if computed != *expected_oid {
            return Err(Error::LooseHashMismatch {
                path: path.display().to_string(),
                real_oid: computed.to_hex(),
            });
        }
        Ok(obj)
    }

    /// Read and decompress an object from the loose store.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] — no file at the expected path.
    /// - [`Error::Zlib`] — decompression failed.
    /// - [`Error::CorruptObject`] — header is malformed.
    pub fn read(&self, oid: &ObjectId) -> Result<Object> {
        if oid.is_well_known_empty_tree() {
            return Ok(crate::objects::Object {
                kind: crate::objects::ObjectKind::Tree,
                data: Vec::new(),
            });
        }

        // Objects written under an active in-memory overlay never hit disk, so they must be
        // resolved here before any loose/pack lookup.
        if let Some(obj) = self.overlay_read(oid) {
            return Ok(obj);
        }

        // Git prepares the packed object store (registering the packs the MIDX names) before
        // serving reads; a MIDX-referenced pack whose `.idx` cannot be opened reports
        // `packfile <pack> index unavailable` even when the requested object turns out to be
        // loose. Reproduce that once-per-process so `rev-list` over a corrupt idx still warns.
        if self.config_git_dir.is_some() && self.core_multi_pack_index_enabled() {
            crate::midx::validate_midx_referenced_packs(&self.objects_dir);
        }

        let path = self.object_path(oid);
        match fs::File::open(&path) {
            Ok(file) => {
                let raw = read_zlib_loose_payload(file)?;
                // Match Git: loose objects are read from the path implied by `oid` without
                // requiring the payload to hash back to that oid (t1006 corrupt-loose / swapped files).
                return parse_object_bytes(&raw);
            }
            Err(_) => {
                // Loose object not found; try pack files.
            }
        }

        if self.config_git_dir.is_some() && self.core_multi_pack_index_enabled() {
            if let Some(obj) = try_read_object_via_midx(&self.objects_dir, oid, self.hash_algo())? {
                return Ok(obj);
            }
        }

        // Fall back to pack files.
        match pack::read_object_from_packs(&self.objects_dir, oid) {
            Ok(obj) => return Ok(obj),
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => return Err(err),
        }

        let midx_alt = self.config_git_dir.is_some() && self.core_multi_pack_index_enabled();

        let file_alts = self.file_alternate_dirs_snapshot();
        for alt_dir in file_alts.iter() {
            if let Ok(obj) = Self::read_from_dir(alt_dir, oid, midx_alt) {
                return Ok(obj);
            }
        }

        for alt_dir in self.env_alternate_dirs_snapshot().iter() {
            if let Ok(obj) = Self::read_from_dir(alt_dir, oid, midx_alt) {
                return Ok(obj);
            }
        }

        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if let Ok(obj) = Self::read_from_dir(alt_dir, oid, false) {
                    return Ok(obj);
                }
            }
        }

        Err(Error::ObjectNotFound(oid.to_hex()))
    }

    /// Return the kind and uncompressed size of `oid` without loading the full object body.
    ///
    /// Resolution order matches [`Self::read`]: in-memory overlay, loose objects, multi-pack-index,
    /// local packs, then alternates.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] — no object with this id in any consulted store.
    /// - [`Error::Zlib`] — decompression failed while reading a loose or pack header.
    /// - [`Error::CorruptObject`] — malformed object or pack headers.
    pub fn read_info(&self, oid: &ObjectId) -> Result<ObjectInfo> {
        if oid.is_well_known_empty_tree() {
            return Ok(ObjectInfo {
                kind: ObjectKind::Tree,
                size: 0,
            });
        }

        if let Some(obj) = self.overlay_read(oid) {
            return Ok(ObjectInfo {
                kind: obj.kind,
                size: u64::try_from(obj.data.len())
                    .map_err(|_| Error::CorruptObject("object size overflow".to_owned()))?,
            });
        }

        if self.config_git_dir.is_some() && self.core_multi_pack_index_enabled() {
            crate::midx::validate_midx_referenced_packs(&self.objects_dir);
        }

        let path = self.object_path(oid);
        if path.is_file() {
            return read_loose_object_info(&path);
        }

        if self.config_git_dir.is_some() && self.core_multi_pack_index_enabled() {
            if let Some(info) = crate::midx::try_read_info_via_midx(&self.objects_dir, oid)? {
                return Ok(info);
            }
        }

        match pack::read_object_info_from_packs(&self.objects_dir, oid) {
            Ok(info) => return Ok(info),
            Err(Error::ObjectNotFound(_)) => {}
            Err(err) => return Err(err),
        }

        let midx_alt = self.config_git_dir.is_some() && self.core_multi_pack_index_enabled();

        let file_alts = self.file_alternate_dirs_snapshot();
        for alt_dir in file_alts.iter() {
            if let Ok(info) = Self::read_info_from_dir(alt_dir, oid, midx_alt) {
                return Ok(info);
            }
        }

        for alt_dir in self.env_alternate_dirs_snapshot().iter() {
            if let Ok(info) = Self::read_info_from_dir(alt_dir, oid, midx_alt) {
                return Ok(info);
            }
        }

        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                if let Ok(info) = Self::read_info_from_dir(alt_dir, oid, false) {
                    return Ok(info);
                }
            }
        }

        Err(Error::ObjectNotFound(oid.to_hex()))
    }

    /// Try to read an object from a specific objects directory (loose or pack).
    fn read_from_dir(objects_dir: &Path, oid: &ObjectId, use_midx: bool) -> Result<Object> {
        let loose = oid.loose_path_in(objects_dir);
        if let Ok(file) = fs::File::open(&loose) {
            let raw = read_zlib_loose_payload(file)?;
            return parse_object_bytes(&raw);
        }
        if use_midx {
            if let Some(obj) =
                try_read_object_via_midx(objects_dir, oid, hash_algo_for_objects_dir(objects_dir))?
            {
                return Ok(obj);
            }
        }
        pack::read_object_from_packs(objects_dir, oid)
    }

    fn read_info_from_dir(
        objects_dir: &Path,
        oid: &ObjectId,
        use_midx: bool,
    ) -> Result<ObjectInfo> {
        let loose = oid.loose_path_in(objects_dir);
        if loose.is_file() {
            return read_loose_object_info(&loose);
        }
        if use_midx {
            if let Some(info) = crate::midx::try_read_info_via_midx(objects_dir, oid)? {
                return Ok(info);
            }
        }
        pack::read_object_info_from_packs(objects_dir, oid)
    }

    /// Hash raw content of a given kind using this repository's hash algorithm.
    ///
    /// This does **not** write anything to disk.
    #[must_use]
    pub fn hash(&self, kind: ObjectKind, data: &[u8]) -> ObjectId {
        hash_object_data_with(self.hash_algo(), kind, data)
    }

    /// Write an object to the loose store and return its [`ObjectId`].
    ///
    /// If the object already exists it is not overwritten (Git behaviour).
    ///
    /// # Errors
    ///
    /// - [`Error::Io`] — could not create the directory or write the file.
    /// - [`Error::Zlib`] — compression failed.
    pub fn write(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        self.write_with_options(kind, data, WriteOptions::default())
    }

    /// Create all 256 loose-object prefix directories (`objects/xx/`).
    ///
    /// Bulk `git add`-style staging calls this once before parallel loose writes.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] if a directory cannot be created.
    pub fn ensure_all_loose_prefix_dirs(&self) -> Result<()> {
        for i in 0u8..=255 {
            let prefix = self.objects_dir.join(format!("{i:02x}"));
            fs::create_dir_all(prefix).map_err(Error::Io)?;
        }
        Ok(())
    }

    /// Zlib-compress canonical loose store bytes using the repository's loose level.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Zlib`] when compression fails.
    pub fn zlib_compress_loose_store(&self, store_bytes: &[u8]) -> Result<Vec<u8>> {
        let compression = self.loose_compression()?;
        zlib_compress_store_bytes(store_bytes, compression)
    }

    /// Publish precompressed bytes to a loose object path via a same-directory temp file.
    ///
    /// Readers never observe a partial object at the final name; failed writes remove the temp.
    fn publish_zlib_loose_object(
        path: &Path,
        prefix_dir: &Path,
        oid: &ObjectId,
        zlib_store: &[u8],
        prefix_dirs_precreated: bool,
    ) -> Result<()> {
        if !prefix_dirs_precreated {
            fs::create_dir_all(prefix_dir).map_err(Error::Io)?;
        }
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        if let Err(e) = fs::write(&tmp_path, zlib_store) {
            let _ = fs::remove_file(&tmp_path);
            return Err(Error::Io(e));
        }
        match fs::rename(&tmp_path, path) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let _ = fs::remove_file(&tmp_path);
            }
            Err(e) => {
                let _ = fs::remove_file(&tmp_path);
                return Err(Error::Io(e));
            }
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o444));
        }
        Ok(())
    }

    /// Write a loose object from precomputed id and zlib-compressed store bytes.
    ///
    /// The caller must supply bytes that the internal zlib loose reader would expand to
    /// valid canonical store form (`"<kind> <len>\\0<payload>"`). Hashing and
    /// compression are skipped; use after parallel batch preparation.
    ///
    /// # Errors
    ///
    /// Same as [`Self::write`].
    pub fn write_loose_zlib_prehashed(
        &self,
        oid: &ObjectId,
        zlib_store: &[u8],
        options: WriteOptions,
    ) -> Result<ObjectId> {
        let path = self.object_path(oid);
        if path.is_file() {
            if !options.silent {
                let _ = self.touch_object_mtime(&path);
            }
            return Ok(*oid);
        }

        let already_exists = if options.trust_new_loose {
            false
        } else if options.assume_loose_only_existence {
            self.exists_local(oid)
        } else {
            self.exists(oid)
        };

        if self.overlay_active() && !already_exists {
            if let Ok(raw) = decompress_zlib_loose_bytes(zlib_store) {
                if let Ok(obj) = parse_object_bytes(&raw) {
                    if self.overlay_store(*oid, obj.kind, &obj.data) {
                        return Ok(*oid);
                    }
                }
            }
        }

        if already_exists {
            if !options.silent {
                let _ = self.freshen_object(oid);
            }
            return Ok(*oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        if !options.trust_new_loose {
            fs::create_dir_all(prefix_dir).map_err(Error::Io)?;
        }

        Self::publish_zlib_loose_object(
            &path,
            prefix_dir,
            oid,
            zlib_store,
            options.trust_new_loose,
        )?;
        Ok(*oid)
    }

    /// Like [`Self::write`], with control over freshen behaviour on existing objects.
    pub fn write_with_options(
        &self,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash_bytes_with(self.hash_algo(), &store_bytes);

        // Cheapest check first: a loose copy in this store answers every case below with a
        // single stat, avoiding the full pack/alternates/MIDX existence scan per write.
        let path = self.object_path(&oid);
        if path.is_file() {
            if !options.silent {
                let _ = self.touch_object_mtime(&path);
            }
            return Ok(oid);
        }

        let already_exists = if options.trust_new_loose {
            false
        } else if options.assume_loose_only_existence {
            self.exists_local(&oid)
        } else {
            self.exists(&oid)
        };

        // When the in-memory overlay is active, keep the object in memory only (unless it is
        // already present on disk, in which case nothing new needs to be written anyway).
        if self.overlay_active() && !already_exists && self.overlay_store(oid, kind, data) {
            return Ok(oid);
        }

        if already_exists {
            if !options.silent {
                let _ = self.freshen_object(&oid);
            }
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let compression = self.loose_compression()?;
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, compression);
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Write an object as a loose file in this object directory only.
    ///
    /// Unlike [`Self::write`], this ignores `info/alternates` and
    /// `GIT_ALTERNATE_OBJECT_DIRECTORIES`: if the object exists only in an
    /// alternate store, it is still written here. That matches how Git's
    /// `unpack-objects` materializes every packed object into the receiving
    /// repository even when the same OID is already reachable via alternates
    /// (see `t5519-push-alternates`).
    ///
    /// The well-known empty tree is still written when no loose file exists yet,
    /// even though [`Self::exists_local`] treats it as virtually present.
    ///
    /// # Errors
    ///
    /// Same as [`Self::write`].
    pub fn write_local(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash_bytes_with(self.hash_algo(), &store_bytes);

        if let Some(existing) = self.try_freshen_existing_local(&oid) {
            return Ok(existing);
        }

        let path = self.object_path(&oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let compression = self.loose_compression()?;
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, compression);
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Write a loose object file when it is missing, even if [`Self::exists`] is true because
    /// the object lives only in a pack.
    ///
    /// Used when materializing a partial-clone layout: objects must be duplicated as loose files
    /// before local packs are removed. Unlike [`Self::write_local`], objects present only in a
    /// promisor pack are still written because [`Self::exists_local`] treats those as absent.
    pub fn write_loose_materialize(&self, kind: ObjectKind, data: &[u8]) -> Result<ObjectId> {
        let store_bytes = build_store_bytes(kind, data);
        let oid = hash_bytes_with(self.hash_algo(), &store_bytes);
        let path = self.object_path(&oid);
        if path.exists() {
            let _ = self.freshen_object(&oid);
            return Ok(oid);
        }

        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let compression = self.loose_compression()?;
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, compression);
            encoder
                .write_all(&store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Write an already-serialized object (header + data) to the loose store.
    ///
    /// Useful when the caller has the full store bytes (e.g. from stdin with
    /// `--literally`).
    ///
    /// # Errors
    ///
    /// - [`Error::CorruptObject`] — the provided bytes don't form a valid header.
    /// - [`Error::Io`] / [`Error::Zlib`] — storage errors.
    pub fn write_raw(&self, store_bytes: &[u8]) -> Result<ObjectId> {
        // Validate the header before storing
        parse_object_bytes(store_bytes)?;

        let oid = hash_bytes_with(self.hash_algo(), store_bytes);
        if let Some(existing) = self.try_freshen_existing(&oid) {
            return Ok(existing);
        }

        let path = self.object_path(&oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let compression = self.loose_compression()?;
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, compression);
            encoder
                .write_all(store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Like [`Self::write_raw`] but only consults this object directory, not alternates.
    ///
    /// See [`Self::write_local`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::write_raw`].
    pub fn write_raw_local(&self, store_bytes: &[u8]) -> Result<ObjectId> {
        parse_object_bytes(store_bytes)?;

        let oid = hash_bytes_with(self.hash_algo(), store_bytes);
        if let Some(existing) = self.try_freshen_existing_local(&oid) {
            return Ok(existing);
        }

        let path = self.object_path(&oid);
        let prefix_dir = path
            .parent()
            .ok_or_else(|| Error::PathError("object path has no parent".to_owned()))?;
        fs::create_dir_all(prefix_dir)?;

        let compression = self.loose_compression()?;
        let tmp_path = oid.loose_tmp_path_in_prefix(prefix_dir);
        {
            let tmp_file = fs::File::create(&tmp_path)?;
            let mut encoder = ZlibEncoder::new(tmp_file, compression);
            encoder
                .write_all(store_bytes)
                .map_err(|e| Error::Zlib(e.to_string()))?;
            encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
        }
        fs::rename(&tmp_path, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o444));
        }

        Ok(oid)
    }

    /// Returns true when a loose object exists at `oid`'s path and zlib-decompresses to a
    /// structurally valid `<type> <size>\0<payload>` object (type may be non-standard).
    ///
    /// Used for `git cat-file -e`, which succeeds for hand-crafted loose objects that
    /// [`Self::read`] rejects due to [`Error::UnknownObjectType`].
    #[must_use]
    pub fn loose_object_plumbing_ok(&self, oid: &ObjectId) -> bool {
        let path = self.object_path(oid);
        let Ok(file) = fs::File::open(&path) else {
            return false;
        };
        let Ok(raw) = read_zlib_loose_payload(file) else {
            return false;
        };
        loose_store_bytes_header_valid(&raw)
    }

    /// Update `path`'s mtime to "now" (Git `utime(path, NULL)`), returning whether it succeeded.
    fn touch_object_mtime(&self, path: &Path) -> Option<std::time::SystemTime> {
        #[cfg(test)]
        self.hot_path_test_metrics.record_freshen();
        // `utime(path, NULL)` sets both atime and mtime to the current time.
        let touched_at = std::time::SystemTime::now();
        let now = filetime::FileTime::from_system_time(touched_at);
        filetime::set_file_times(path, now, now)
            .ok()
            .map(|()| touched_at)
    }
}

fn loose_store_bytes_header_valid(raw: &[u8]) -> bool {
    let nul = match raw.iter().position(|&b| b == 0) {
        Some(i) => i,
        None => return false,
    };
    let header = &raw[..nul];
    let data = &raw[nul + 1..];
    let sp = match header.iter().position(|&b| b == b' ') {
        Some(i) => i,
        None => return false,
    };
    if sp == 0 || sp > 32 {
        return false;
    }
    let size_str = match std::str::from_utf8(&header[sp + 1..]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let size: usize = match size_str.parse() {
        Ok(s) => s,
        Err(_) => return false,
    };
    data.len() == size
}

/// Hash the canonical store bytes of an object (`"<kind> <len>\0<data>"`) with
/// the given hash algorithm.
fn hash_object_data_with(algo: HashAlgo, kind: ObjectKind, data: &[u8]) -> ObjectId {
    hash::hash_object(algo, kind, data)
}

/// Compute the digest of pre-built store bytes with the given hash algorithm.
fn hash_bytes_with(algo: HashAlgo, data: &[u8]) -> ObjectId {
    algo.digest(data)
}

/// Build canonical blob store bytes (`"blob <len>\\0<payload>"`).
pub(crate) fn blob_store_bytes(data: &[u8]) -> Vec<u8> {
    build_store_bytes(ObjectKind::Blob, data)
}

pub(crate) fn zlib_compress_loose_store_from_bytes(
    store_bytes: &[u8],
    compression: flate2::Compression,
) -> Result<Vec<u8>> {
    zlib_compress_store_bytes(store_bytes, compression)
}

fn zlib_compress_store_bytes(
    store_bytes: &[u8],
    compression: flate2::Compression,
) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut encoder = ZlibEncoder::new(&mut out, compression);
    encoder
        .write_all(store_bytes)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    encoder.finish().map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(out)
}

fn decompress_zlib_loose_bytes(zlib: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(zlib);
    let mut raw = Vec::new();
    decoder
        .read_to_end(&mut raw)
        .map_err(|e| Error::Zlib(e.to_string()))?;
    Ok(raw)
}

/// Build the canonical store byte sequence: `"<kind> <len>\0<data>"`.
fn build_store_bytes(kind: ObjectKind, data: &[u8]) -> Vec<u8> {
    let header = format!("{} {}\0", kind, data.len());
    let mut out = Vec::with_capacity(header.len() + data.len());
    out.extend_from_slice(header.as_bytes());
    out.extend_from_slice(data);
    out
}

/// Parse decompressed object bytes (`"<type> <size>\0<data>"`) into an [`Object`].
pub(crate) fn parse_object_bytes(raw: &[u8]) -> Result<Object> {
    parse_object_bytes_inner(raw, None)
}

pub(crate) fn parse_object_bytes_with_oid(raw: &[u8], oid: &ObjectId) -> Result<Object> {
    parse_object_bytes_inner(raw, Some(oid))
}

/// Parse `"<type> <size>\0"` from decompressed bytes that include at least the header prefix.
pub(crate) fn parse_object_header_prefix(raw: &[u8]) -> Result<(ObjectKind, u64)> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;

    let header = &raw[..nul];
    let sp = header
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject("missing space in object header".to_owned()))?;

    if sp > 32 {
        return Err(Error::ObjectHeaderTooLong {
            oid: hash_bytes_with(HashAlgo::Sha1, raw).to_hex(),
        });
    }

    let kind = ObjectKind::from_bytes(&header[..sp])?;
    let size_str = std::str::from_utf8(&header[sp + 1..])
        .map_err(|_| Error::CorruptObject("non-UTF-8 object size".to_owned()))?;
    let size: u64 = size_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("invalid object size: {size_str}")))?;
    Ok((kind, size))
}

/// Read kind and size from a loose object file without loading the full payload.
pub(crate) fn read_loose_object_info(path: &Path) -> Result<ObjectInfo> {
    let file = fs::File::open(path).map_err(Error::Io)?;
    let mut decoder = ZlibDecoder::new(file);
    let mut prefix = Vec::with_capacity(64);
    let mut buf = [0u8; 256];
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| Error::Zlib(e.to_string()))?;
        if n == 0 {
            break;
        }
        prefix.extend_from_slice(&buf[..n]);
        if prefix.contains(&0) || prefix.len() >= 128 {
            break;
        }
    }
    let (kind, size) = parse_object_header_prefix(&prefix)?;
    Ok(ObjectInfo { kind, size })
}

fn parse_object_bytes_inner(raw: &[u8], oid_hint: Option<&ObjectId>) -> Result<Object> {
    let nul = raw
        .iter()
        .position(|&b| b == 0)
        .ok_or_else(|| Error::CorruptObject("missing NUL in object header".to_owned()))?;

    let header = &raw[..nul];
    let data = raw[nul + 1..].to_vec();

    let sp = header
        .iter()
        .position(|&b| b == b' ')
        .ok_or_else(|| Error::CorruptObject("missing space in object header".to_owned()))?;

    if sp > 32 {
        let oid_str = oid_hint
            .map(|o| o.to_hex())
            .unwrap_or_else(|| hash_bytes_with(HashAlgo::Sha1, raw).to_hex());
        return Err(Error::ObjectHeaderTooLong { oid: oid_str });
    }

    let kind = ObjectKind::from_bytes(&header[..sp])?;

    let size_str = std::str::from_utf8(&header[sp + 1..])
        .map_err(|_| Error::CorruptObject("non-UTF-8 object size".to_owned()))?;
    let size: usize = size_str
        .parse()
        .map_err(|_| Error::CorruptObject(format!("invalid object size: {size_str}")))?;

    if data.len() != size {
        return Err(Error::CorruptObject(format!(
            "object size mismatch: header says {size} but got {}",
            data.len()
        )));
    }

    Ok(Object::new(kind, data))
}

/// Parse a colon-separated alternates string, handling double-quoted entries
/// with octal escape sequences.
fn parse_alternate_env(val: &str) -> Vec<PathBuf> {
    let mut result = Vec::new();
    let mut chars = val.chars().peekable();
    while chars.peek().is_some() {
        if chars.peek() == Some(&':') {
            chars.next();
            continue;
        }
        if chars.peek() == Some(&'"') {
            // Try quoted parsing; if EOF is hit without closing quote,
            // fall back to treating the whole segment as a raw path.
            chars.next(); // consume the opening '"'
            let saved: Vec<char> = chars.clone().collect();
            let mut path = String::new();
            let mut properly_closed = false;
            loop {
                match chars.next() {
                    None => break,
                    Some('"') => {
                        properly_closed = true;
                        break;
                    }
                    Some('\\') => match chars.peek() {
                        Some(c) if c.is_ascii_digit() => {
                            let mut oct = String::new();
                            for _ in 0..3 {
                                if let Some(&c) = chars.peek() {
                                    if c.is_ascii_digit() {
                                        oct.push(c);
                                        chars.next();
                                    } else {
                                        break;
                                    }
                                } else {
                                    break;
                                }
                            }
                            if let Ok(byte) = u8::from_str_radix(&oct, 8) {
                                path.push(byte as char);
                            }
                        }
                        Some(_) => {
                            if let Some(c) = chars.next() {
                                match c {
                                    'n' => path.push('\n'),
                                    't' => path.push('\t'),
                                    'r' => path.push('\r'),
                                    _ => path.push(c),
                                }
                            }
                        }
                        None => {}
                    },
                    Some(c) => path.push(c),
                }
            }
            if !properly_closed {
                // Broken quoting: fall back to treating raw value (with leading ")
                // as a literal path.
                let raw: String = std::iter::once('"').chain(saved).collect();
                // Extract up to ':' or end
                let raw_path = raw.split(':').next().unwrap_or(&raw);
                if !raw_path.is_empty() {
                    result.push(PathBuf::from(raw_path));
                }
                // Advance past the ':' in the original chars (we consumed the saved copy)
                // Since chars is now at EOF, we need to handle remaining items.
                // Actually, we consumed chars fully. Let's reconstruct from raw.
                let remainder = &raw[raw_path.len()..];
                if let Some(rest) = remainder.strip_prefix(':') {
                    // Parse remaining entries
                    result.extend(parse_alternate_env(rest));
                }
                return result;
            } else if !path.is_empty() {
                result.push(PathBuf::from(path));
            }
        } else {
            let mut path = String::new();
            while let Some(&c) = chars.peek() {
                if c == ':' {
                    break;
                }
                path.push(c);
                chars.next();
            }
            if !path.is_empty() {
                result.push(PathBuf::from(path));
            }
        }
    }
    result
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use tempfile::TempDir;

    #[test]
    fn round_trip_blob() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let data = b"hello world";
        let oid = odb.write(ObjectKind::Blob, data).unwrap();
        let obj = odb.read(&oid).unwrap();
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert_eq!(obj.data, data);
    }

    #[test]
    fn known_blob_hash() {
        // Verified: echo -n "hello" | git hash-object --stdin
        //        => b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0
        let oid = HashAlgo::Sha1.hash_object(ObjectKind::Blob, b"hello");
        assert_eq!(oid.to_hex(), "b6fc4c620b67d95f953a5c1c1230aaab5db5a1b0");
    }

    fn git_dir_with_config(config: &str) -> (TempDir, PathBuf, PathBuf) {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join(".git");
        let objects = git_dir.join("objects");
        fs::create_dir_all(&objects).unwrap();
        fs::write(git_dir.join("config"), config).unwrap();
        (dir, git_dir, objects)
    }

    /// Zlib FLEVEL flag (upper two bits of the second header byte).
    fn zlib_flevel(compressed: &[u8]) -> u8 {
        assert!(compressed.len() >= 2);
        (compressed[1] >> 6) & 3
    }

    fn tmp_loose_object_paths(objects: &Path) -> Vec<PathBuf> {
        let mut tmp = Vec::new();
        let Ok(shards) = fs::read_dir(objects) else {
            return tmp;
        };
        for shard in shards.flatten() {
            if !shard.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let Ok(files) = fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let name = file.file_name();
                if name.to_string_lossy().starts_with("tmp_") {
                    tmp.push(file.path());
                }
            }
        }
        tmp
    }

    #[test]
    fn loose_write_honours_level() {
        let payload: Vec<u8> = (0..4096).map(|i| (i % 251) as u8).collect();

        let (_dir, git_dir, objects) = git_dir_with_config("");
        let odb = Odb::new(&objects).with_config_git_dir(git_dir.clone());
        let oid_default = odb.write(ObjectKind::Blob, &payload).unwrap();
        let bytes_default = fs::read(odb.object_path(&oid_default)).unwrap();
        assert!(zlib_flevel(&bytes_default) <= 1);
        assert_eq!(odb.read(&oid_default).unwrap().data, payload);

        for level in -1i32..=9 {
            let config = format!("[core]\n\tloosecompression = {level}\n");
            let (_dir, git_dir, objects) = git_dir_with_config(&config);
            let odb = Odb::new(&objects).with_config_git_dir(git_dir);
            let byte = u8::try_from(level.rem_euclid(256)).unwrap_or(0);
            let bytes: Vec<u8> = vec![byte; 512];
            let oid = odb.write(ObjectKind::Blob, &bytes).unwrap();
            let on_disk = fs::read(odb.object_path(&oid)).unwrap();
            assert!(on_disk.len() >= 2, "level {level}");
            assert_eq!(odb.read(&oid).unwrap().data, bytes.as_slice());
        }

        let config = "[core]\n\tcompression = 9\n";
        let (_dir, git_dir, objects) = git_dir_with_config(config);
        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        let oid = odb.write(ObjectKind::Blob, &payload).unwrap();
        assert_eq!(zlib_flevel(&fs::read(odb.object_path(&oid)).unwrap()), 3);

        let config = "[core]\n\tcompression = 0\n\tloosecompression = 1\n";
        let (_dir, git_dir, objects) = git_dir_with_config(config);
        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        let oid = odb.write(ObjectKind::Blob, &payload).unwrap();
        assert_eq!(zlib_flevel(&fs::read(odb.object_path(&oid)).unwrap()), 0);

        let config = "[core]\n\tloosecompression = 99\n";
        let (_dir, git_dir, objects) = git_dir_with_config(config);
        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        assert!(odb.write(ObjectKind::Blob, b"x").is_err());
        assert!(tmp_loose_object_paths(&objects).is_empty());
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn write_loose_zlib_prehashed_matches_write() {
        let dir = TempDir::new().unwrap();
        let objects = dir.path().join("objects");
        let odb = Odb::new(&objects);
        let data = b"parallel prehash path";
        let oid_normal = odb.write(ObjectKind::Blob, data).unwrap();

        let dir2 = TempDir::new().unwrap();
        let objects2 = dir2.path().join("objects");
        let odb2 = Odb::new(&objects2);
        let store = super::blob_store_bytes(data);
        let oid = hash::hash_object(odb2.hash_algo(), ObjectKind::Blob, data);
        assert_eq!(oid, oid_normal);
        let zlib = odb2.zlib_compress_loose_store(&store).unwrap();
        let oid2 = odb2
            .write_loose_zlib_prehashed(
                &oid,
                &zlib,
                WriteOptions {
                    assume_loose_only_existence: true,
                    ..WriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(oid2, oid_normal);
        assert_eq!(
            fs::read(odb2.object_path(&oid2)).unwrap(),
            fs::read(odb.object_path(&oid_normal)).unwrap()
        );
    }

    /// Re-writing an object that already exists as a loose file must not replace it; Git
    /// only touches mtime via [`Self::freshen_object`].
    #[test]
    fn write_existing_loose_does_not_rewrite() {
        let dir = TempDir::new().unwrap();
        let objects = dir.path().join("objects");
        fs::create_dir_all(&objects).unwrap();
        let odb = Odb::new(&objects);
        let payload = b"already loose";
        let oid = odb.write(ObjectKind::Blob, payload).unwrap();
        let path = odb.object_path(&oid);
        let before_bytes = fs::read(&path).unwrap();
        let meta_before = fs::metadata(&path).unwrap();
        let before_mtime = filetime::FileTime::from_last_modification_time(&meta_before);
        #[cfg(unix)]
        let ino_before = {
            use std::os::unix::fs::MetadataExt;
            meta_before.ino()
        };
        std::thread::sleep(std::time::Duration::from_millis(20));
        let oid2 = odb.write(ObjectKind::Blob, payload).unwrap();
        assert_eq!(oid, oid2);
        assert_eq!(fs::read(&path).unwrap(), before_bytes);
        let meta_after = fs::metadata(&path).unwrap();
        let after_mtime = filetime::FileTime::from_last_modification_time(&meta_after);
        assert!(
            after_mtime >= before_mtime,
            "expected freshen to bump or preserve mtime"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(
                meta_after.ino(),
                ino_before,
                "rewrite must not replace the loose object file"
            );
        }
    }

    /// Objects present only in a pack must not be duplicated as loose files on [`Self::write`].
    #[test]
    fn write_skips_loose_when_object_in_pack_only() {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("f"), b"pack-only payload").unwrap();
        git_in(dir.path(), &["add", "f"]);
        git_in(dir.path(), &["commit", "-m", "c"]);
        git_in(dir.path(), &["repack", "-ad"]);
        let objects = dir.path().join(".git").join("objects");
        let odb = Odb::new(&objects);
        let oid = odb.write(ObjectKind::Blob, b"pack-only payload").unwrap();
        assert!(!odb.object_path(&oid).exists());
        assert!(odb.exists(&oid));
    }

    /// Same as pack-only: alternates satisfy [`Self::exists`] without a local loose copy.
    #[test]
    fn write_freshens_loose_object_in_alternate() {
        use filetime::FileTime;
        use std::thread;
        use std::time::Duration;

        let alt = TempDir::new().unwrap();
        git_in(alt.path(), &["init", "-q", "--bare"]);
        git_in(alt.path(), &["config", "user.email", "t@example.com"]);
        let alt_objects = alt.path().join("objects");
        let alt_odb = Odb::new(&alt_objects);
        let payload = b"alternate-loose";
        let oid = alt_odb.write(ObjectKind::Blob, payload).unwrap();
        let alt_loose = alt_odb.object_path(&oid);

        let primary = TempDir::new().unwrap();
        let primary_objects = primary.path().join("objects");
        fs::create_dir_all(primary_objects.join("info")).unwrap();
        fs::write(
            primary_objects.join("info").join("alternates"),
            format!("{}\n", alt_objects.display()),
        )
        .unwrap();
        let odb = Odb::new(&primary_objects);
        thread::sleep(Duration::from_millis(50));
        let before = FileTime::from_last_modification_time(&fs::metadata(&alt_loose).unwrap());
        filetime::set_file_mtime(&alt_loose, before).unwrap();

        let oid2 = odb.write(ObjectKind::Blob, payload).unwrap();
        assert_eq!(oid, oid2);
        assert!(!odb.object_path(&oid).exists());
        let after = FileTime::from_last_modification_time(&fs::metadata(&alt_loose).unwrap());
        assert!(after > before, "write must freshen alternate loose object");
    }

    #[test]
    fn write_freshens_packed_object_in_alternate() {
        use filetime::FileTime;
        use std::thread;
        use std::time::Duration;

        let alt = TempDir::new().unwrap();
        git_in(alt.path(), &["init", "-q"]);
        git_in(alt.path(), &["config", "user.email", "t@example.com"]);
        git_in(alt.path(), &["config", "user.name", "T"]);
        let payload = b"alternate-pack";
        fs::write(alt.path().join("f"), payload).unwrap();
        git_in(alt.path(), &["add", "f"]);
        git_in(alt.path(), &["commit", "-m", "c"]);
        git_in(alt.path(), &["repack", "-ad"]);
        let alt_objects = alt.path().join(".git").join("objects");
        let alt_odb = Odb::new(&alt_objects);
        let oid = alt_odb.write(ObjectKind::Blob, payload).unwrap();
        let pack_path = fs::read_dir(alt_objects.join("pack"))
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .unwrap()
            .path();
        thread::sleep(Duration::from_millis(50));
        let before = FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());

        let primary = TempDir::new().unwrap();
        let primary_objects = primary.path().join("objects");
        fs::create_dir_all(primary_objects.join("info")).unwrap();
        fs::write(
            primary_objects.join("info").join("alternates"),
            format!("{}\n", alt_objects.display()),
        )
        .unwrap();
        pack::clear_pack_cache();
        let odb = Odb::new(&primary_objects);
        assert!(odb.exists(&oid));
        let oid2 = odb.write(ObjectKind::Blob, payload).unwrap();
        assert_eq!(oid, oid2);
        assert!(!odb.object_path(&oid).exists());
        let after = FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());
        assert!(after >= before, "write must freshen alternate pack");
    }

    #[test]
    fn write_skips_loose_when_object_only_in_alternate() {
        let alt = TempDir::new().unwrap();
        git_in(alt.path(), &["init", "-q", "--bare"]);
        git_in(alt.path(), &["config", "user.email", "t@example.com"]);
        let alt_objects = alt.path().join("objects");
        let alt_odb = Odb::new(&alt_objects);
        let payload = b"alternate-only";
        let oid = alt_odb.write(ObjectKind::Blob, payload).unwrap();

        let primary = TempDir::new().unwrap();
        let primary_objects = primary.path().join("objects");
        fs::create_dir_all(primary.path().join("objects").join("info")).unwrap();
        fs::write(
            primary
                .path()
                .join("objects")
                .join("info")
                .join("alternates"),
            format!("{}\n", alt_objects.display()),
        )
        .unwrap();
        let odb = Odb::new(&primary_objects);
        assert!(odb.exists(&oid));
        let oid2 = odb.write(ObjectKind::Blob, payload).unwrap();
        assert_eq!(oid, oid2);
        assert!(!odb.object_path(&oid).exists());
    }

    #[test]
    fn write_with_mem_overlay_stays_in_memory() {
        let dir = TempDir::new().unwrap();
        let objects = dir.path().join("objects");
        fs::create_dir_all(&objects).unwrap();
        let odb = Odb::new(&objects);
        odb.enable_mem_overlay();
        let payload = b"overlay blob";
        let oid = odb.write(ObjectKind::Blob, payload).unwrap();
        assert!(!odb.object_path(&oid).exists());
        assert_eq!(odb.read(&oid).unwrap().data, payload);
        odb.disable_mem_overlay();
        assert!(odb.read(&oid).is_err());
    }

    #[test]
    fn read_alternates_relative_path_and_symlinked_objects_dir() {
        use crate::pack::read_alternates_recursive;

        let layout = TempDir::new().unwrap();
        let alt_objects = layout.path().join("alt-store").join("objects");
        fs::create_dir_all(&alt_objects).unwrap();
        let primary_objects = layout.path().join("repo").join("objects");
        fs::create_dir_all(primary_objects.join("info")).unwrap();
        fs::write(
            primary_objects.join("info").join("alternates"),
            "../../alt-store/objects\n",
        )
        .unwrap();
        let resolved = read_alternates_recursive(&primary_objects).unwrap();
        let alt_canonical = fs::canonicalize(&alt_objects).unwrap_or(alt_objects.clone());
        assert!(
            resolved
                .iter()
                .any(|p| { fs::canonicalize(p).unwrap_or_else(|_| p.clone()) == alt_canonical }),
            "relative alternate entry should resolve: {resolved:?}"
        );

        #[cfg(unix)]
        {
            let link_parent = TempDir::new().unwrap();
            std::os::unix::fs::symlink(&primary_objects, link_parent.path().join("objects-link"))
                .unwrap();
            let via_link =
                read_alternates_recursive(&link_parent.path().join("objects-link")).unwrap();
            assert!(
                !via_link.is_empty(),
                "alternates through symlinked objects dir: {via_link:?}"
            );
        }
    }
    #[test]
    fn empty_tree_exists_without_hex() {
        let odb = Odb::new(std::path::Path::new("/nonexistent/objects"));
        let canon = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf8d69288fbee4904").unwrap();
        let legacy = ObjectId::from_hex("4b825dc642cb6eb9a060e54bf899d69f7c6948d4").unwrap();
        assert!(odb.exists(&canon));
        assert!(odb.exists(&legacy));
        assert!(odb.exists_local(&canon));
        assert!(!odb.freshen_object(&canon));
    }

    #[test]
    fn append_alternate_objects_line_errors_on_unreadable_existing_file() {
        let dir = TempDir::new().unwrap();
        let objects = dir.path().join("objects");
        fs::create_dir_all(objects.join("info")).unwrap();
        let alt_path = objects.join("info/alternates");
        fs::write(&alt_path, b"not valid utf-8 \xff\n").unwrap();
        let err = Odb::append_alternate_objects_line(&objects, Path::new("/tmp/other/objects"))
            .unwrap_err();
        assert!(matches!(err, Error::Io(_)));
        assert_eq!(fs::read(&alt_path).unwrap(), b"not valid utf-8 \xff\n");
    }

    #[test]
    fn append_file_alternate_refreshes_cached_snapshot() {
        let primary = TempDir::new().unwrap();
        let alternate = TempDir::new().unwrap();
        let alt_objects = alternate.path().join("objects");
        std::fs::create_dir_all(&alt_objects).unwrap();
        let alt_odb = Odb::new(&alt_objects);
        let oid = alt_odb.write(ObjectKind::Blob, b"via-alternate").unwrap();

        let primary_objects = primary.path().join("objects");
        std::fs::create_dir_all(primary_objects.join("info")).unwrap();
        let primary_odb = Odb::new(&primary_objects);
        assert!(!primary_odb.exists(&oid));
        let _ = primary_odb.file_alternate_dirs_snapshot();
        primary_odb.append_file_alternate(&alt_objects).unwrap();
        assert!(primary_odb.exists(&oid));
    }

    #[test]
    fn exists_via_env_alternate_dirs_at_construction() {
        let primary = TempDir::new().unwrap();
        let alternate = TempDir::new().unwrap();
        let alt_objects = alternate.path().join("objects");
        std::fs::create_dir_all(&alt_objects).unwrap();
        let alt_odb = Odb::new(&alt_objects);
        let oid = alt_odb.write(ObjectKind::Blob, b"env-alt").unwrap();

        let primary_objects = primary.path().join("objects");
        std::fs::create_dir_all(&primary_objects).unwrap();
        let primary_odb = Odb::new(&primary_objects).with_env_alternate_dirs(vec![alt_objects]);
        assert!(primary_odb.exists(&oid));
    }

    #[test]
    fn write_existing_loose_uses_path_stat_not_exists() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let oid = odb.write(ObjectKind::Blob, b"once").unwrap();
        let path = odb.object_path(&oid);
        assert!(path.is_file());
        odb.reset_exists_probe();
        let oid2 = odb.write(ObjectKind::Blob, b"once").unwrap();
        assert_eq!(oid, oid2);
        assert_eq!(
            odb.exists_probe_count(),
            0,
            "loose fast path must not call exists()"
        );
    }

    #[test]
    fn write_uses_single_exists_probe_when_not_loose() {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("f"), b"probe-once").unwrap();
        git_in(dir.path(), &["add", "f"]);
        git_in(dir.path(), &["commit", "-m", "c"]);
        git_in(dir.path(), &["repack", "-ad"]);
        let objects = dir.path().join(".git").join("objects");
        let odb = Odb::new(&objects);
        let oid = odb.write(ObjectKind::Blob, b"probe-once").unwrap();
        assert!(!odb.object_path(&oid).exists());
        odb.reset_exists_probe();
        let oid2 = odb.write(ObjectKind::Blob, b"probe-once").unwrap();
        assert_eq!(oid, oid2);
        assert_eq!(
            odb.exists_probe_count(),
            1,
            "pack/alternate write path must call exists() exactly once"
        );
    }
    #[test]
    fn write_existing_object_freshens() {
        use filetime::FileTime;
        use std::fs;
        use std::thread;
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let oid = odb.write(ObjectKind::Blob, b"hello").unwrap();
        let path = odb.object_path(&oid);
        thread::sleep(Duration::from_millis(50));
        let before = FileTime::from_last_modification_time(&fs::metadata(&path).unwrap());
        filetime::set_file_mtime(&path, before).unwrap();

        let _ = odb.write(ObjectKind::Blob, b"hello").unwrap();
        let after = FileTime::from_last_modification_time(&fs::metadata(&path).unwrap());
        assert!(
            after > before,
            "rewriting an existing loose object must freshen its mtime"
        );
    }

    #[test]
    fn freshen_pack_once_per_odb() {
        use crate::objects::HashAlgo;
        use filetime::FileTime;
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::fs;
        use std::io::Write;
        use std::thread;
        use std::time::Duration;

        fn append_pack_object(buf: &mut Vec<u8>, type_code: u8, data: &[u8]) {
            let mut size = data.len();
            let first = ((type_code & 0x7) << 4) | (size & 0x0f) as u8;
            size >>= 4;
            if size > 0 {
                buf.push(first | 0x80);
                while size > 0 {
                    let b = (size & 0x7f) as u8;
                    size >>= 7;
                    buf.push(if size > 0 { b | 0x80 } else { b });
                }
            } else {
                buf.push(first);
            }
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(data).unwrap();
            buf.extend_from_slice(&enc.finish().unwrap());
        }

        fn write_v2_idx(
            idx_path: &Path,
            pack_path: &Path,
            entries: &[(ObjectId, u64)],
        ) -> Result<()> {
            let mut sorted = entries.to_vec();
            sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            let n = sorted.len();
            let mut fanout = [0u32; 256];
            for byte in 0u32..256 {
                let count = sorted
                    .iter()
                    .filter(|(oid, _)| u32::from(oid.as_bytes()[0]) <= byte)
                    .count();
                fanout[byte as usize] = u32::try_from(count).unwrap_or(u32::MAX);
            }
            let mut buf = Vec::new();
            buf.extend_from_slice(b"\xfftOc");
            buf.extend_from_slice(&2u32.to_be_bytes());
            for f in fanout {
                buf.extend_from_slice(&f.to_be_bytes());
            }
            for (oid, _) in &sorted {
                buf.extend_from_slice(oid.as_bytes());
            }
            for _ in 0..n {
                buf.extend_from_slice(&0u32.to_be_bytes());
            }
            for (_, off) in &sorted {
                buf.extend_from_slice(&u32::try_from(*off).unwrap_or(0x8000_0000).to_be_bytes());
            }
            let pack_bytes = fs::read(pack_path)?;
            buf.extend_from_slice(&pack_bytes[pack_bytes.len() - 20..]);
            buf.extend_from_slice(HashAlgo::Sha1.digest(&buf).as_bytes());
            fs::write(idx_path, buf)?;
            Ok(())
        }

        let dir = TempDir::new().unwrap();
        let objects_dir = dir.path();
        let odb = Odb::new(objects_dir);

        let oid_a = odb.write(ObjectKind::Blob, b"alpha").unwrap();
        let oid_b = odb.write(ObjectKind::Blob, b"beta").unwrap();
        fs::remove_file(odb.object_path(&oid_a)).unwrap();
        fs::remove_file(odb.object_path(&oid_b)).unwrap();

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&2u32.to_be_bytes());
        let off_a = pack.len() as u64;
        append_pack_object(&mut pack, 3, b"alpha");
        let off_b = pack.len() as u64;
        append_pack_object(&mut pack, 3, b"beta");
        pack.extend_from_slice(HashAlgo::Sha1.digest(&pack).as_bytes());

        let pack_dir = objects_dir.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let pack_path = pack_dir.join("test-177.pack");
        let idx_path = pack_dir.join("test-177.idx");
        fs::write(&pack_path, &pack).unwrap();
        write_v2_idx(&idx_path, &pack_path, &[(oid_a, off_a), (oid_b, off_b)]).unwrap();
        pack::clear_pack_cache();

        thread::sleep(Duration::from_millis(50));
        let before = FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());

        assert!(odb.freshen_object(&oid_a));
        let after_first = FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());
        assert!(after_first > before);

        assert!(odb.freshen_object(&oid_b));
        let after_second =
            FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());
        assert_eq!(
            after_second, after_first,
            "second freshen of another object in the same pack must not touch the pack again"
        );
    }

    #[test]
    fn write_existing_packed_object_freshens_pack() {
        use crate::objects::HashAlgo;
        use filetime::FileTime;
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::fs;
        use std::io::Write;
        use std::thread;
        use std::time::Duration;

        let dir = TempDir::new().unwrap();
        let objects_dir = dir.path();
        let odb = Odb::new(objects_dir);
        let oid = odb.write(ObjectKind::Blob, b"packed-only").unwrap();
        fs::remove_file(odb.object_path(&oid)).unwrap();

        let mut pack = Vec::new();
        pack.extend_from_slice(b"PACK");
        pack.extend_from_slice(&2u32.to_be_bytes());
        pack.extend_from_slice(&1u32.to_be_bytes());
        let off = pack.len() as u64;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"packed-only").unwrap();
        let compressed = enc.finish().unwrap();
        pack.push(0x30 | 0x03); // blob len 11
        pack.extend_from_slice(&compressed);
        pack.extend_from_slice(HashAlgo::Sha1.digest(&pack).as_bytes());

        let pack_dir = objects_dir.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let pack_path = pack_dir.join("single-177.pack");
        fs::write(&pack_path, &pack).unwrap();
        // Minimal idx: reuse logic from freshen_pack_once_per_odb via inline write
        let idx_path = pack_dir.join("single-177.idx");
        let mut sorted = [(oid, off)];
        sorted.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
        let mut fanout = [0u32; 256];
        for byte in 0u32..256 {
            fanout[byte as usize] = if u32::from(oid.as_bytes()[0]) <= byte {
                1
            } else {
                0
            };
        }
        let mut buf = Vec::new();
        buf.extend_from_slice(b"\xfftOc");
        buf.extend_from_slice(&2u32.to_be_bytes());
        for f in fanout {
            buf.extend_from_slice(&f.to_be_bytes());
        }
        buf.extend_from_slice(oid.as_bytes());
        buf.extend_from_slice(&0u32.to_be_bytes());
        buf.extend_from_slice(&(off as u32).to_be_bytes());
        buf.extend_from_slice(&pack[pack.len() - 20..]);
        buf.extend_from_slice(HashAlgo::Sha1.digest(&buf).as_bytes());
        fs::write(&idx_path, buf).unwrap();
        pack::clear_pack_cache();

        thread::sleep(Duration::from_millis(50));
        let before = FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());

        odb.write(ObjectKind::Blob, b"packed-only").unwrap();

        let after = FileTime::from_last_modification_time(&fs::metadata(&pack_path).unwrap());
        assert!(
            after > before,
            "write of an existing packed object must freshen the pack mtime"
        );
    }

    /// When a local pack is indexed but its `.pack` path cannot be utime'd, [`Odb::write`]
    /// materializes a loose copy (Git `write_object_file` fallback). Replacing the pack file
    /// with a directory simulates a failed freshen without requiring a root-owned pack.
    #[test]
    fn write_materializes_loose_when_pack_freshen_fails() {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        let payload = b"packed-fallback";
        fs::write(dir.path().join("f"), payload).unwrap();
        git_in(dir.path(), &["add", "f"]);
        git_in(dir.path(), &["commit", "-m", "c"]);
        git_in(dir.path(), &["repack", "-ad"]);
        let objects = dir.path().join(".git").join("objects");
        let odb = Odb::new(&objects);
        let oid = odb.write(ObjectKind::Blob, payload).unwrap();
        assert!(!odb.object_path(&oid).exists());
        assert!(odb.exists(&oid));

        let pack_path = fs::read_dir(objects.join("pack"))
            .unwrap()
            .flatten()
            .find(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .unwrap()
            .path();
        fs::remove_file(&pack_path).unwrap();
        fs::create_dir(&pack_path).unwrap();
        pack::clear_pack_cache();
        assert!(!odb.freshen_object(&oid));

        odb.write(ObjectKind::Blob, payload).unwrap();
        assert!(odb.object_path(&oid).exists());
    }
    #[test]
    fn write_with_options_silent_skips_freshen_on_existing_loose_object() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let oid = odb.write(ObjectKind::Blob, b"stable").unwrap();
        let path = odb.object_path(&oid);
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let again = odb
            .write_with_options(
                ObjectKind::Blob,
                b"stable",
                WriteOptions {
                    silent: true,
                    ..WriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(oid, again);
        let after = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(before, after);

        let touched = odb
            .write_with_options(
                ObjectKind::Blob,
                b"stable",
                WriteOptions {
                    silent: false,
                    ..WriteOptions::default()
                },
            )
            .unwrap();
        assert_eq!(oid, touched);
        let freshened = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(freshened >= before);
    }

    #[test]
    fn promisor_marker_checked_once_per_prepare() {
        use crate::objects::ObjectId;
        use crate::pack::{
            clear_pack_cache, pack_cache_test_guard, test_pack_marker_stat_count,
            test_reset_pack_marker_stat_count,
        };

        let _guard = pack_cache_test_guard();
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        for i in 0..64 {
            fs::write(dir.path().join(format!("f{i}.txt")), format!("body {i}")).unwrap();
            git_in(dir.path(), &["add", "."]);
            git_in(dir.path(), &["commit", "-m", &format!("c{i}")]);
        }
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let head_hex = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let oid =
            ObjectId::from_hex(std::str::from_utf8(&head_hex.stdout).unwrap().trim()).unwrap();

        clear_pack_cache();
        test_reset_pack_marker_stat_count();
        let odb = Odb::new(&objects);
        assert!(odb.exists_local(&oid));
        let pack_count = fs::read_dir(objects.join("pack"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .count();
        assert_eq!(
            test_pack_marker_stat_count(),
            u64::try_from(pack_count * 2).unwrap(),
            "prepare should stat promisor and mtimes once per pack"
        );

        test_reset_pack_marker_stat_count();
        for _ in 0..10_000 {
            assert!(odb.exists_local(&oid));
        }
        assert_eq!(
            test_pack_marker_stat_count(),
            0,
            "cached sidecar flags must not stat markers on each exists_local probe"
        );
    }

    #[test]
    fn promisor_marker_added_after_prepare_is_seen_after_reprepare() {
        use crate::objects::ObjectId;
        use crate::pack::{
            clear_pack_cache, pack_cache_test_guard, reprepare_pack_directory_on_miss,
        };

        let _guard = pack_cache_test_guard();
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("one.txt"), b"x").unwrap();
        git_in(dir.path(), &["add", "one.txt"]);
        git_in(dir.path(), &["commit", "-m", "one"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let head_hex = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let oid =
            ObjectId::from_hex(std::str::from_utf8(&head_hex.stdout).unwrap().trim()).unwrap();

        clear_pack_cache();
        let odb = Odb::new(&objects);
        assert!(odb.exists_local(&oid));

        let pack_path = fs::read_dir(objects.join("pack"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "pack"))
            .unwrap();
        fs::write(pack_path.with_extension("promisor"), b"").unwrap();
        filetime::set_file_mtime(objects.join("pack"), filetime::FileTime::now()).unwrap();

        assert!(
            reprepare_pack_directory_on_miss(&objects).unwrap(),
            "pack directory change must require reprepare"
        );
        assert!(
            !odb.exists_local(&oid),
            "promisor marker must hide pack objects after reprepare refreshed sidecar flags"
        );
    }

    #[test]
    fn git_promisor_marker_pack_not_materialized_for_exists_local() {
        use crate::objects::ObjectId;
        use crate::pack::clear_pack_cache;

        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("blob.txt"), b"git promisor compat").unwrap();
        git_in(dir.path(), &["add", "blob.txt"]);
        git_in(dir.path(), &["commit", "-m", "seed"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let head_hex = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["rev-parse", "HEAD"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        let oid =
            ObjectId::from_hex(std::str::from_utf8(&head_hex.stdout).unwrap().trim()).unwrap();
        let pack_path = fs::read_dir(objects.join("pack"))
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|x| x == "pack"))
            .unwrap();
        fs::write(pack_path.with_extension("promisor"), b"").unwrap();

        clear_pack_cache();
        let odb = Odb::new(&objects);
        assert!(
            !odb.exists_local(&oid),
            "git-written pack with .promisor must not count as local materialization"
        );

        let cat = std::process::Command::new("git")
            .current_dir(dir.path())
            .args(["cat-file", "-e", &oid.to_hex()])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .unwrap();
        assert!(
            cat.status.success(),
            "git still resolves the object in a promisor-marked pack"
        );
    }
}
