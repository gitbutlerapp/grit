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

pub mod store;

pub(crate) use store::loose::{
    build_store_bytes, decompress_zlib_loose_bytes, enumerate_loose_objects,
    for_each_loose_object_id, loose_store_bytes_header_valid, parse_object_bytes,
    read_loose_object_info, read_zlib_loose_payload, zlib_compress_store_bytes,
};
pub use store::LooseStore;
pub use store::{CompositeStore, FilesSource, ObjectStream};
use store::{MemoryStore, ObjectStore, WritableObjectStore};

use std::collections::{HashMap, HashSet};
use std::fs;
use std::hash::{Hash, Hasher};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use crate::config::ConfigSet;
use crate::diagnostics::NullDiagnostics;
use crate::error::{Error, Result};
use crate::hash;
use crate::midx::validate_midx_referenced_packs_with_diagnostics;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::pack;
use crate::pack_store::PackStore;
use flate2::Compression;

type MemOdbOverlayStore = Arc<RwLock<Option<Arc<MemoryStore>>>>;

/// Cached alternate [`CompositeStore`] keyed by file/env/submodule alternate state.
struct AlternateSourcesCache {
    file_generation: u64,
    env_fingerprint: u64,
    submodule_generation: u64,
    sources: Option<Arc<CompositeStore>>,
}

struct AlternateSourcesKey {
    file_generation: u64,
    env_fingerprint: u64,
    submodule_generation: u64,
}

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

fn objects_dir_has_pack_index_files(objects_dir: &Path) -> bool {
    let pack_dir = objects_dir.join("pack");
    let Ok(entries) = fs::read_dir(pack_dir) else {
        return false;
    };
    entries
        .filter_map(|e| e.ok())
        .any(|ent| ent.path().extension().is_some_and(|ext| ext == "idx"))
}

fn object_in_local_packs(objects_dir: &Path, oid: &ObjectId) -> bool {
    object_in_local_packs_filtered(objects_dir, oid, false)
}

fn object_in_local_packs_filtered(
    objects_dir: &Path,
    oid: &ObjectId,
    include_promisor: bool,
) -> bool {
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return false;
    };
    for idx in &indexes {
        if !include_promisor && idx.is_promisor {
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
    /// Whether `objects/pack` contains at least one `.idx` file (directory probe, once per Odb).
    local_pack_indexes_seen: Arc<OnceLock<bool>>,
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
    mem_overlay: MemOdbOverlayStore,
    /// Whether [`Self::enable_mem_overlay`] activated the in-memory overlay (avoids locking on reads).
    mem_overlay_active: Arc<AtomicBool>,
    /// Lazily built [`FilesSource`] for [`Self::objects_dir`].
    primary_source: Arc<RwLock<Option<Arc<FilesSource>>>>,
    /// Alternate object dirs as a [`CompositeStore`] (generation-bumped with file alternates).
    alternate_sources_cache: Arc<RwLock<AlternateSourcesCache>>,
    /// Whether MIDX pack validation has run for this [`Odb`] (Git warns once per process).
    midx_packs_validated: Arc<OnceLock<()>>,
    /// The repository's object hash algorithm (`extensions.objectformat`),
    /// detected lazily from the config and cached. Determines the hash used
    /// when writing objects. Defaults to SHA-1 when no config is available.
    hash_algo_cache: Arc<OnceLock<HashAlgo>>,
    /// Zlib level for loose-object writes, resolved from config and cached.
    loose_zlib_cache: Arc<OnceLock<Compression>>,
    /// Shared with [`crate::repo::Repository`] so config is loaded once per open handle.
    shared_config_state: Option<crate::repo::RepositoryConfigSnapshot>,
    /// One-time sync of `core.deltaBaseCacheLimit` into the process-wide pack delta-base LRU.
    delta_base_cache_sync: Arc<OnceLock<()>>,
    /// Environment paired with [`Self::shared_config_state`] for repository-scoped config cache.
    shared_environment: Option<Arc<crate::environment::Environment>>,
    /// Fallback when [`Self::shared_environment`] is unset (repo-local config only).
    empty_config_env: Arc<OnceLock<crate::environment::Environment>>,
    /// Pack files whose mtimes were already bumped for object freshening on this [`Odb`]
    /// (Git's `packed_git->freshened`: at most one `utimensat` per pack per process).
    freshened_packs: Arc<Mutex<HashSet<PathBuf>>>,
    /// Repository-scoped pack/MIDX read caches (shared by [`Odb`] clones).
    pack_store: Arc<PackStore>,
    /// Cached [`PackStore`] handles for alternate object directories (lazy).
    alternate_pack_stores: Arc<Mutex<HashMap<PathBuf, Arc<PackStore>>>>,
    /// Bumps when [`Self::register_submodule_object_directories_from_index`] repopulates submodule sources.
    submodule_sources_generation: Arc<AtomicU64>,
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
pub fn hash_algo_for_git_dir(env: &crate::environment::Environment, git_dir: &Path) -> HashAlgo {
    ConfigSet::load(env, Some(git_dir), true)
        .ok()
        .and_then(|cfg| cfg.get("extensions.objectformat"))
        .and_then(|v| HashAlgo::from_name(&v))
        .unwrap_or(HashAlgo::Sha1)
}

/// Like [`hash_algo_for_git_dir`] but takes an `objects/` directory path.
#[must_use]
pub fn hash_algo_for_objects_dir(
    env: &crate::environment::Environment,
    objects_dir: &Path,
) -> HashAlgo {
    objects_dir
        .parent()
        .map(|d| hash_algo_for_git_dir(env, d))
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
            local_pack_indexes_seen: Arc::new(OnceLock::new()),
            file_alternate_dirs_cache: Arc::new(RwLock::new(FileAlternatesCache {
                generation: 0,
                snapshot: None,
            })),
            env_alternate_dirs: Vec::new(),
            env_alternate_lazy: Arc::new(OnceLock::new()),
            mem_overlay: Arc::new(RwLock::new(None)),
            mem_overlay_active: Arc::new(AtomicBool::new(false)),
            primary_source: Arc::new(RwLock::new(None)),
            alternate_sources_cache: Arc::new(RwLock::new(AlternateSourcesCache {
                file_generation: 0,
                env_fingerprint: 0,
                submodule_generation: 0,
                sources: None,
            })),
            midx_packs_validated: Arc::new(OnceLock::new()),
            hash_algo_cache: Arc::new(OnceLock::new()),
            loose_zlib_cache: Arc::new(OnceLock::new()),
            shared_config_state: None,
            delta_base_cache_sync: Arc::new(OnceLock::new()),
            shared_environment: None,
            empty_config_env: Arc::new(OnceLock::new()),
            freshened_packs: Arc::new(Mutex::new(HashSet::new())),
            pack_store: Arc::new(PackStore::new(objects_dir.to_path_buf())),
            alternate_pack_stores: Arc::new(Mutex::new(HashMap::new())),
            submodule_sources_generation: Arc::new(AtomicU64::new(0)),
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
            local_pack_indexes_seen: Arc::new(OnceLock::new()),
            file_alternate_dirs_cache: Arc::new(RwLock::new(FileAlternatesCache {
                generation: 0,
                snapshot: None,
            })),
            env_alternate_dirs: Vec::new(),
            env_alternate_lazy: Arc::new(OnceLock::new()),
            mem_overlay: Arc::new(RwLock::new(None)),
            mem_overlay_active: Arc::new(AtomicBool::new(false)),
            primary_source: Arc::new(RwLock::new(None)),
            alternate_sources_cache: Arc::new(RwLock::new(AlternateSourcesCache {
                file_generation: 0,
                env_fingerprint: 0,
                submodule_generation: 0,
                sources: None,
            })),
            midx_packs_validated: Arc::new(OnceLock::new()),
            hash_algo_cache: Arc::new(OnceLock::new()),
            loose_zlib_cache: Arc::new(OnceLock::new()),
            shared_config_state: None,
            delta_base_cache_sync: Arc::new(OnceLock::new()),
            shared_environment: None,
            empty_config_env: Arc::new(OnceLock::new()),
            freshened_packs: Arc::new(Mutex::new(HashSet::new())),
            pack_store: Arc::new(PackStore::new(objects_dir.to_path_buf())),
            alternate_pack_stores: Arc::new(Mutex::new(HashMap::new())),
            submodule_sources_generation: Arc::new(AtomicU64::new(0)),
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

    #[cfg(test)]
    #[must_use]
    pub fn hot_path_test_metrics_arc(
        &self,
    ) -> Arc<crate::hot_path_test_metrics::HotPathTestMetrics> {
        Arc::clone(&self.hot_path_test_metrics)
    }

    /// Share the repository's lazy config snapshot (see [`crate::repo::Repository::config`]).
    #[must_use]
    pub(crate) fn with_shared_config_state(
        mut self,
        state: crate::repo::RepositoryConfigSnapshot,
        env: Arc<crate::environment::Environment>,
    ) -> Self {
        self.shared_config_state = Some(state);
        self.shared_environment = Some(env);
        self
    }

    fn diagnostics_handle(&self) -> crate::diagnostics::DiagnosticsHandle {
        self.shared_config_state
            .as_ref()
            .map(|state| state.diagnostics_handle())
            .unwrap_or_else(|| Arc::new(NullDiagnostics))
    }

    fn load_config_cascade(&self) -> Result<ConfigSet> {
        if let (Some(caches), Some(env)) = (&self.shared_config_state, &self.shared_environment) {
            let git_dir = self
                .config_git_dir
                .as_deref()
                .or_else(|| self.objects_dir.parent());
            let cfg = caches.load_config(env.as_ref(), git_dir, true)?;
            return Ok(cfg.as_ref().clone());
        }
        let git_dir = self
            .config_git_dir
            .as_deref()
            .or_else(|| self.objects_dir.parent());
        if let Some(git_dir) = git_dir {
            ConfigSet::load(self.config_environment(), Some(git_dir), true)
        } else {
            Ok(ConfigSet::new())
        }
    }

    fn config_environment(&self) -> &crate::environment::Environment {
        if let Some(env) = self.shared_environment.as_deref() {
            return env;
        }
        self.empty_config_env
            .get_or_init(crate::environment::Environment::empty)
    }

    fn env_alternate_dirs_snapshot(&self) -> Arc<Vec<PathBuf>> {
        if !self.env_alternate_dirs.is_empty() {
            return Arc::new(self.env_alternate_dirs.clone());
        }
        Arc::clone(self.env_alternate_lazy.get_or_init(|| {
            Arc::new(Self::env_alternate_dirs_from_var(
                self.config_environment(),
                self.work_tree.as_deref(),
            ))
        }))
    }

    /// Parse `GIT_ALTERNATE_OBJECT_DIRECTORIES` once for [`Self::with_env_alternate_dirs`].
    ///
    /// Relative entries are resolved against `resolve_base` (typically the work tree root).
    #[must_use]
    pub fn env_alternate_dirs_from_var(
        env: &crate::environment::Environment,
        resolve_base: Option<&Path>,
    ) -> Vec<PathBuf> {
        match env.var("GIT_ALTERNATE_OBJECT_DIRECTORIES") {
            Some(val) if !val.is_empty() => {
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
        self.clear_alternate_sources_cache();
        self
    }

    fn clear_alternate_sources_cache(&self) {
        if let Ok(mut guard) = self.alternate_sources_cache.write() {
            guard.sources = None;
        }
    }

    fn alternate_sources_key(&self) -> AlternateSourcesKey {
        AlternateSourcesKey {
            file_generation: self
                .file_alternate_dirs_cache
                .read()
                .map(|g| g.generation)
                .unwrap_or(0),
            env_fingerprint: self.env_alternate_fingerprint(),
            submodule_generation: self.submodule_sources_generation.load(Ordering::Relaxed),
        }
    }

    fn env_alternate_fingerprint(&self) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for path in self.env_alternate_dirs_snapshot().iter() {
            path.hash(&mut hasher);
        }
        hasher.finish()
    }

    /// Drop the cached `info/alternates` chain so the next lookup re-reads the file.
    pub fn invalidate_alternates_cache(&self) {
        if let Ok(mut guard) = self.file_alternate_dirs_cache.write() {
            guard.generation = guard.generation.wrapping_add(1);
            guard.snapshot = None;
        }
        self.clear_alternate_sources_cache();
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
        self.clear_alternate_sources_cache();
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
        self.mem_overlay_active.store(true, Ordering::Release);
        if let Ok(mut guard) = self.mem_overlay.write() {
            *guard = Some(Arc::new(MemoryStore::new(self.hash_algo())));
        }
    }

    /// Disable the in-memory write overlay, discarding any objects accumulated in it.
    pub fn disable_mem_overlay(&self) {
        self.mem_overlay_active.store(false, Ordering::Release);
        if let Ok(mut guard) = self.mem_overlay.write() {
            *guard = None;
        }
    }

    /// Whether the in-memory write overlay is currently enabled.
    fn overlay_active(&self) -> bool {
        self.mem_overlay
            .read()
            .ok()
            .and_then(|g| g.as_ref().map(|_| ()))
            .is_some()
    }

    fn overlay_store(&self) -> Option<Arc<MemoryStore>> {
        if !self.mem_overlay_active.load(Ordering::Acquire) {
            return None;
        }
        self.mem_overlay.read().ok().and_then(|g| g.clone())
    }

    /// Number of objects stored in the active mem overlay (tests only).
    #[cfg(test)]
    pub(crate) fn mem_overlay_len_for_tests(&self) -> Option<usize> {
        let store = self.overlay_store()?;
        let mut count = 0usize;
        store
            .for_each_object(&mut |_| {
                count += 1;
                ControlFlow::Continue(())
            })
            .ok()?;
        Some(count)
    }

    /// If the in-memory overlay is active, store `(kind, data)` under `oid` there and return
    /// `true`; otherwise return `false` so the caller falls through to the on-disk path.
    fn overlay_store_object(&self, oid: ObjectId, kind: ObjectKind, data: &[u8]) -> bool {
        let Some(store) = self.overlay_store() else {
            return false;
        };
        store.insert_object(oid, kind, Arc::from(data.to_owned()));
        true
    }

    /// Read `oid` from the in-memory overlay, if active and present.
    fn overlay_read(&self, oid: &ObjectId) -> Option<Object> {
        let store = self.overlay_store()?;
        store.read(oid).ok().flatten()
    }

    /// Primary on-disk object source for this repository's `objects/` directory.
    ///
    /// # Errors
    ///
    /// Propagates failures while opening pack, MIDX, or loose backends.
    pub fn primary(&self) -> Result<Arc<FilesSource>> {
        if let Ok(guard) = self.primary_source.read() {
            if let Some(src) = guard.as_ref() {
                return Ok(Arc::clone(src));
            }
        }
        let src = self.build_files_source(
            &self.objects_dir,
            self.multi_pack_index_reads_enabled(),
            self.midx_tip_listing_enabled(),
        )?;
        if let Ok(mut guard) = self.primary_source.write() {
            *guard = Some(Arc::clone(&src));
        }
        Ok(src)
    }

    /// Alternate object directories (`info/alternates`, env, submodules) as one composite store.
    ///
    /// # Errors
    ///
    /// Propagates failures while opening any alternate files source.
    pub fn sources(&self) -> Result<Arc<CompositeStore>> {
        let key = self.alternate_sources_key();
        if let Ok(guard) = self.alternate_sources_cache.read() {
            if guard.file_generation == key.file_generation
                && guard.env_fingerprint == key.env_fingerprint
                && guard.submodule_generation == key.submodule_generation
            {
                if let Some(sources) = guard.sources.as_ref() {
                    return Ok(Arc::clone(sources));
                }
            }
        }
        let use_midx = self.multi_pack_index_reads_enabled();
        let midx_tip = self.midx_tip_listing_enabled();
        let mut stores: Vec<Arc<dyn ObjectStore>> = Vec::new();
        for alt_dir in self.file_alternate_dirs_snapshot().iter() {
            stores.push(self.build_files_source(alt_dir, use_midx, midx_tip)?);
        }
        for alt_dir in self.env_alternate_dirs_snapshot().iter() {
            stores.push(self.build_files_source(alt_dir, use_midx, midx_tip)?);
        }
        if let Ok(guard) = self.submodule_alternate_dirs.lock() {
            for alt_dir in guard.iter() {
                stores.push(self.build_files_source(alt_dir, false, false)?);
            }
        }
        let composite = Arc::new(CompositeStore::new(stores, self.hash_algo()));
        if let Ok(mut guard) = self.alternate_sources_cache.write() {
            guard.file_generation = key.file_generation;
            guard.env_fingerprint = key.env_fingerprint;
            guard.submodule_generation = key.submodule_generation;
            guard.sources = Some(Arc::clone(&composite));
        }
        Ok(composite)
    }

    fn midx_tip_listing_enabled(&self) -> bool {
        self.config_git_dir.is_some() && self.core_multi_pack_index_enabled()
    }

    fn build_files_source(
        &self,
        objects_dir: &Path,
        use_midx: bool,
        midx_tip_listing_for_exists: bool,
    ) -> Result<Arc<FilesSource>> {
        let loose = self.loose_at(objects_dir)?;
        let pack_store = self.pack_store_for(objects_dir);
        let diagnostics = if objects_dir == self.objects_dir.as_path() {
            self.diagnostics_handle()
        } else {
            Arc::new(NullDiagnostics)
        };
        FilesSource::open(
            pack_store,
            loose,
            use_midx,
            midx_tip_listing_for_exists,
            diagnostics,
        )
        .map(Arc::new)
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
        self.submodule_sources_generation
            .fetch_add(1, Ordering::Relaxed);
        self.clear_alternate_sources_cache();
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

    fn sync_delta_base_cache_limit(&self) {
        self.delta_base_cache_sync.get_or_init(|| {
            if self.config_git_dir.is_some() {
                let cfg = self.load_config_cascade().unwrap_or_default();
                self.pack_store.configure_delta_base_from_config(Some(&cfg));
            }
        });
    }

    fn pack_store_for(&self, objects_dir: &Path) -> Arc<PackStore> {
        if objects_dir == self.objects_dir.as_path() {
            return Arc::clone(&self.pack_store);
        }
        if let Ok(mut guard) = self.alternate_pack_stores.lock() {
            if let Some(store) = guard.get(objects_dir) {
                return Arc::clone(store);
            }
            let store = Arc::new(PackStore::new(objects_dir.to_path_buf()));
            guard.insert(objects_dir.to_path_buf(), Arc::clone(&store));
            return store;
        }
        Arc::new(PackStore::new(objects_dir.to_path_buf()))
    }

    /// Pack/MIDX read cache shared by this handle and its [`Clone`]s.
    #[must_use]
    pub fn pack_store(&self) -> &Arc<PackStore> {
        &self.pack_store
    }

    /// Drop cached pack listings, parsed indexes, pack bytes, delta bases, and MIDX layers.
    ///
    /// Call after in-process repack, garbage collection, or [`crate::index_pack::install_pack_bytes`]
    /// so the next read rescans `objects/pack/`. Cloned [`Odb`] handles share one
    /// [`PackStore`] for the primary `objects/` directory; alternates have separate stores
    /// that are invalidated here as well.
    pub fn invalidate_packs(&self) {
        self.pack_store.invalidate_all();
        if let Ok(guard) = self.alternate_pack_stores.lock() {
            for store in guard.values() {
                store.invalidate_all();
            }
        }
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

    fn multi_pack_index_reads_enabled(&self) -> bool {
        if self.config_git_dir.is_none() {
            return false;
        }
        if !*self
            .local_pack_indexes_seen
            .get_or_init(|| objects_dir_has_pack_index_files(&self.objects_dir))
        {
            return false;
        }
        self.core_multi_pack_index_enabled()
    }

    fn ensure_midx_prepared(&self) {
        if !self.multi_pack_index_reads_enabled() {
            return;
        }
        let _ = self.midx_packs_validated.get_or_init(|| {
            let diagnostics = self.diagnostics_handle();
            validate_midx_referenced_packs_with_diagnostics(
                &self.objects_dir,
                diagnostics.as_ref(),
            );
        });
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
        self.with_pack_store_for(&self.objects_dir, || {
            exists_materialized_in_objects_dir(&self.objects_dir, oid)
        })
    }

    /// Check whether an object exists in the loose store or any pack file.
    #[must_use]
    pub fn exists(&self, oid: &ObjectId) -> bool {
        #[cfg(test)]
        self.exists_probe.fetch_add(1, Ordering::Relaxed);
        if oid.is_well_known_empty_tree() {
            return true;
        }
        if self.overlay_read(oid).is_some() {
            return true;
        }
        if self
            .primary()
            .ok()
            .and_then(|primary| primary.contains(oid).ok())
            == Some(true)
        {
            return true;
        }
        self.sources()
            .ok()
            .and_then(|sources| sources.contains(oid).ok())
            == Some(true)
    }

    /// Run `f` with this [`Odb`]'s [`PackStore`] for `objects_dir` (primary or alternate).
    fn with_pack_store_for<R>(&self, objects_dir: &Path, f: impl FnOnce() -> R) -> R {
        let store = self.pack_store_for(objects_dir);
        PackStore::with_context(store, f)
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
        self.with_pack_store_for(objects_dir, || {
            let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
                return false;
            };
            for idx in &indexes {
                if idx.contains(oid) {
                    return self.freshen_pack_once(&idx.pack_path, idx.is_cruft);
                }
            }
            false
        })
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
        if self.with_pack_store_for(&self.objects_dir, || {
            exists_materialized_in_objects_dir(&self.objects_dir, oid)
        }) && self.freshen_object(oid)
        {
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
        LooseStore::read_verify_oid(path, expected_oid)
    }

    fn local_loose(&self) -> Result<LooseStore> {
        Ok(LooseStore::new(
            self.objects_dir.clone(),
            self.hash_algo(),
            self.loose_compression()?,
        ))
    }

    fn loose_at(&self, objects_dir: &Path) -> Result<LooseStore> {
        Ok(LooseStore::new(
            objects_dir.to_path_buf(),
            self.hash_algo(),
            self.loose_compression()?,
        ))
    }

    /// Read and decompress an object from the loose store.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] — no file at the expected path.
    /// - [`Error::Zlib`] — decompression failed.
    /// - [`Error::CorruptObject`] — header is malformed.
    pub fn read(&self, oid: &ObjectId) -> Result<Object> {
        if let Some(obj) = self.overlay_read(oid) {
            return Ok(obj);
        }

        let use_midx = self.multi_pack_index_reads_enabled();
        if use_midx {
            self.ensure_midx_prepared();
        }

        self.sync_delta_base_cache_limit();

        let primary = self.primary()?;
        let mut unreadable_local_loose = None;
        match primary.read(oid) {
            Ok(Some(obj)) => return Ok(obj),
            Ok(None) => {}
            Err(err) if primary.local_loose_exists_but_unreadable(oid, &err) => {
                unreadable_local_loose = Some(err);
            }
            Err(err) => return Err(err),
        }

        if let Ok(Some(obj)) = self.sources()?.read(oid) {
            return Ok(obj);
        }

        if let Some(err) = unreadable_local_loose {
            return Err(err);
        }

        Err(Error::ObjectNotFound(oid.to_hex()))
    }

    /// Return the kind and uncompressed size of `oid` without loading the full object body.
    ///
    /// Resolution order matches [`Self::read`]: in-memory overlay, multi-pack-index, local packs
    /// (MRU order, skipping MIDX-covered indexes when MIDX is enabled), loose objects, a pack
    /// directory re-prepare and pack retry, then alternates.
    ///
    /// # Errors
    ///
    /// - [`Error::ObjectNotFound`] — no object with this id in any consulted store.
    /// - [`Error::Zlib`] — decompression failed while reading a loose or pack header.
    /// - [`Error::CorruptObject`] — malformed object or pack headers.
    pub fn read_info(&self, oid: &ObjectId) -> Result<ObjectInfo> {
        if let Some(obj) = self.overlay_read(oid) {
            return Ok(ObjectInfo {
                kind: obj.kind,
                size: u64::try_from(obj.data.len())
                    .map_err(|_| Error::CorruptObject("object size overflow".to_owned()))?,
            });
        }

        let use_midx = self.multi_pack_index_reads_enabled();
        if use_midx {
            self.ensure_midx_prepared();
        }

        let primary = self.primary()?;
        let mut unreadable_local_loose = None;
        match primary.read_info(oid) {
            Ok(Some(info)) => return Ok(info),
            Ok(None) => {}
            Err(err) if primary.local_loose_exists_but_unreadable(oid, &err) => {
                unreadable_local_loose = Some(err);
            }
            Err(err) => return Err(err),
        }

        if let Ok(Some(info)) = self.sources()?.read_info(oid) {
            return Ok(info);
        }

        if let Some(err) = unreadable_local_loose {
            return Err(err);
        }

        Err(Error::ObjectNotFound(oid.to_hex()))
    }

    /// Open a stream over `oid`'s uncompressed payload (overlay, primary, then alternates).
    ///
    /// # Errors
    ///
    /// Propagates backend failures other than a missing object (`Ok(None)`).
    pub fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'static>>> {
        use std::io::Cursor;
        if let Some(obj) = self.overlay_read(oid) {
            let size = u64::try_from(obj.data.len()).map_err(|_| {
                Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
            })?;
            return Ok(Some(ObjectStream {
                kind: obj.kind,
                size,
                reader: Box::new(Cursor::new(obj.data)),
            }));
        }
        if self.multi_pack_index_reads_enabled() {
            self.ensure_midx_prepared();
        }
        self.sync_delta_base_cache_limit();
        let primary = self.primary()?;
        let mut unreadable_local_loose = None;
        match primary.open_stream(oid) {
            Ok(Some(stream)) => return materialize_stream(stream),
            Ok(None) => {}
            Err(err) if primary.local_loose_exists_but_unreadable(oid, &err) => {
                unreadable_local_loose = Some(err);
            }
            Err(err) => return Err(err),
        }
        match self.sources()?.open_stream(oid) {
            Ok(Some(stream)) => return materialize_stream(stream),
            Ok(None) => {}
            Err(err) => return Err(err),
        }
        if let Some(err) = unreadable_local_loose {
            return Err(err);
        }
        Ok(None)
    }

    /// Invoke `f` for every object id reachable from this database until `f` returns [`ControlFlow::Break`].
    ///
    /// # Errors
    ///
    /// Propagates enumeration failures from any consulted backend.
    pub fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let mut seen = HashSet::new();
        let mut visit = |store: &dyn ObjectStore| -> Result<bool> {
            store::for_each_propagate_break(store, &mut |oid| {
                if !seen.insert(*oid) {
                    return ControlFlow::Continue(());
                }
                f(oid)
            })
        };
        if let Some(overlay) = self.overlay_store() {
            if visit(overlay.as_ref())? {
                return Ok(());
            }
        }
        if visit(self.primary()?.as_ref())? {
            return Ok(());
        }
        let _ = visit(self.sources()?.as_ref())?;
        Ok(())
    }

    /// Fill `out` with ids whose hex starts with `prefix` (deduplicated across overlay, primary, alternates).
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] when `prefix` is not valid hex.
    pub fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let mut seen: HashSet<ObjectId> = out.iter().copied().collect();
        if let Some(overlay) = self.overlay_store() {
            append_lookup_prefix_layer(overlay.as_ref(), prefix, limit, out, &mut seen)?;
            if limit != 0 && out.len() >= limit {
                return Ok(());
            }
        }
        append_lookup_prefix_layer(self.primary()?.as_ref(), prefix, limit, out, &mut seen)?;
        if limit != 0 && out.len() >= limit {
            return Ok(());
        }
        append_lookup_prefix_layer(self.sources()?.as_ref(), prefix, limit, out, &mut seen)
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
        self.local_loose()?.ensure_all_loose_prefix_dirs()
    }

    /// Zlib-compress canonical loose store bytes using the repository's loose level.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Zlib`] when compression fails.
    pub fn zlib_compress_loose_store(&self, store_bytes: &[u8]) -> Result<Vec<u8>> {
        self.local_loose()?.zlib_compress_store_bytes(store_bytes)
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
                    if self.overlay_store_object(*oid, obj.kind, &obj.data) {
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

        self.local_loose()?
            .write_zlib_prehashed(oid, zlib_store, options)
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
        if self.overlay_active() && !already_exists && self.overlay_store_object(oid, kind, data) {
            return Ok(oid);
        }

        if already_exists {
            if !options.silent {
                let _ = self.freshen_object(&oid);
            }
            return Ok(oid);
        }

        self.local_loose()?.write(kind, data, options)
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

        self.local_loose()?
            .write(kind, data, WriteOptions::default())
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

        self.local_loose()?
            .write(kind, data, WriteOptions::default())
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

        self.local_loose()?
            .write_store_prehashed(&oid, store_bytes, WriteOptions::default())
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

        self.local_loose()?
            .write_store_prehashed(&oid, store_bytes, WriteOptions::default())
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
        let now = filetime::FileTime::now();
        filetime::set_file_times(path, now, now).ok()?;
        Some(
            std::time::UNIX_EPOCH
                + std::time::Duration::new(now.unix_seconds() as u64, now.nanoseconds()),
        )
    }
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
#[allow(dead_code)]
pub(crate) fn blob_store_bytes(data: &[u8]) -> Vec<u8> {
    build_store_bytes(ObjectKind::Blob, data)
}

#[allow(dead_code)]
pub(crate) fn zlib_compress_loose_store_from_bytes(
    store_bytes: &[u8],
    compression: flate2::Compression,
) -> Result<Vec<u8>> {
    zlib_compress_store_bytes(store_bytes, compression)
}

fn append_lookup_prefix_layer(
    store: &dyn ObjectStore,
    prefix: &str,
    limit: usize,
    out: &mut Vec<ObjectId>,
    seen: &mut HashSet<ObjectId>,
) -> Result<()> {
    let mut scratch = Vec::new();
    store.lookup_prefix(prefix, 0, &mut scratch)?;
    for oid in scratch {
        if seen.insert(oid) {
            out.push(oid);
            if limit != 0 && out.len() >= limit {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn materialize_stream(stream: ObjectStream<'_>) -> Result<Option<ObjectStream<'static>>> {
    use std::io::{Cursor, Read};
    let ObjectStream {
        kind,
        size,
        mut reader,
    } = stream;
    let mut payload = Vec::new();
    reader.read_to_end(&mut payload).map_err(Error::Io)?;
    Ok(Some(ObjectStream {
        kind,
        size,
        reader: Box::new(Cursor::new(payload)),
    }))
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

    #[test]
    fn parse_object_header_prefix_unknown_kind_is_typed_error() {
        let err = crate::odb::store::loose::parse_object_header_prefix(b"not-a-git-kind 0\0")
            .unwrap_err();
        assert!(matches!(err, Error::UnknownObjectType(_)));
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

    fn git_run(dir: &Path, args: &[&str]) -> std::process::Output {
        std::process::Command::new("git") // hygiene: git fixture comparison in cfg(test) module
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .unwrap()
    }

    fn git_in(dir: &Path, args: &[&str]) {
        let out = git_run(dir, args);
        assert!(
            out.status.success(),
            "git {:?}: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn git_rev_parse_oid(dir: &Path, spec: &str) -> ObjectId {
        let out = git_run(dir, &["rev-parse", spec]);
        assert!(
            out.status.success(),
            "git rev-parse {spec}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        ObjectId::from_hex(std::str::from_utf8(&out.stdout).unwrap().trim()).unwrap()
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
    fn exists_and_write_after_invalidate_packs_use_odb_store() {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        let payload = b"invalidate-pack-exists";
        fs::write(dir.path().join("f"), payload).unwrap();
        git_in(dir.path(), &["add", "f"]);
        git_in(dir.path(), &["commit", "-m", "c"]);
        git_in(dir.path(), &["repack", "-ad"]);
        let objects = dir.path().join(".git").join("objects");
        let odb = Odb::new(&objects);
        let oid = odb.hash(ObjectKind::Blob, payload);
        assert!(odb.exists(&oid), "blob must live in pack after repack");
        let pack_dir = objects.join("pack");
        for entry in fs::read_dir(&pack_dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "pack" || e == "idx") {
                fs::remove_file(path).unwrap();
            }
        }
        odb.invalidate_packs();
        assert!(
            !odb.exists(&oid),
            "exists must not read stale legacy pack listing after invalidate_packs"
        );
        let oid2 = odb.write(ObjectKind::Blob, payload).unwrap();
        assert_eq!(oid, oid2);
        assert!(
            odb.object_path(&oid).is_file(),
            "write must materialize loose object when pack copy is gone"
        );
        odb.read(&oid).expect("read after write");
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
        odb.invalidate_packs();

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
        odb.invalidate_packs();

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
        odb.invalidate_packs();
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
        let oid = git_rev_parse_oid(dir.path(), "HEAD");

        clear_pack_cache();
        test_reset_pack_marker_stat_count();
        let pack_count = fs::read_dir(objects.join("pack"))
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().is_some_and(|x| x == "pack"))
            .count();
        let expected_prepare_stats = u64::try_from(pack_count * 2).unwrap();
        let odb = Odb::new(&objects);
        let before_prepare = test_pack_marker_stat_count();
        assert!(odb.exists_local(&oid));
        assert_eq!(
            test_pack_marker_stat_count().saturating_sub(before_prepare),
            expected_prepare_stats,
            "prepare should stat promisor and mtimes once per pack"
        );

        test_reset_pack_marker_stat_count();
        for _ in 0..10_000 {
            let before_probe = test_pack_marker_stat_count();
            assert!(odb.exists_local(&oid));
            assert_eq!(
                test_pack_marker_stat_count(),
                before_probe,
                "cached sidecar flags must not stat markers on each exists_local probe"
            );
        }
    }

    #[test]
    fn promisor_marker_added_after_prepare_is_seen_after_reprepare() {
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
        let oid = git_rev_parse_oid(dir.path(), "HEAD");

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
            odb.with_pack_store_for(&objects, || {
                reprepare_pack_directory_on_miss(&objects).unwrap()
            }),
            "pack directory change must require reprepare"
        );
        assert!(
            !odb.exists_local(&oid),
            "promisor marker must hide pack objects after reprepare refreshed sidecar flags"
        );
    }

    #[test]
    fn git_promisor_marker_pack_not_materialized_for_exists_local() {
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
        let oid = git_rev_parse_oid(dir.path(), "HEAD");
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

        let cat = git_run(dir.path(), &["cat-file", "-e", &oid.to_hex()]);
        assert!(
            cat.status.success(),
            "git still resolves the object in a promisor-marked pack"
        );
    }

    #[test]
    fn packed_object_read_avoids_loose_path_open() {
        use crate::hot_path_test_metrics::HotPathMetricsScope;
        use crate::pack::clear_pack_cache;

        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("packed.txt"), b"packed-only payload").unwrap();
        git_in(dir.path(), &["add", "packed.txt"]);
        git_in(dir.path(), &["commit", "-m", "pack"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let oid = git_rev_parse_oid(dir.path(), "HEAD");

        clear_pack_cache();
        let odb = Odb::new(&objects);
        odb.hot_path_test_metrics().set_loose_open_counting(true);
        odb.hot_path_test_metrics().reset_loose_path_open_attempts();
        let _scope = HotPathMetricsScope::install(Arc::clone(&odb.hot_path_test_metrics));

        let obj = odb.read(&oid).expect("packed read");
        assert_eq!(obj.kind, ObjectKind::Commit);
        assert_eq!(
            odb.hot_path_test_metrics().loose_path_open_attempts(),
            0,
            "packed objects must not attempt a loose-path open"
        );
    }

    #[test]
    fn loose_and_packed_copies_return_identical_bytes() {
        use crate::pack::clear_pack_cache;

        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("dup.txt"), b"duplicate loose and packed\n").unwrap();
        git_in(dir.path(), &["add", "dup.txt"]);
        git_in(dir.path(), &["commit", "-m", "seed"]);
        let objects = dir.path().join(".git/objects");
        let tree_oid = git_rev_parse_oid(dir.path(), "HEAD^{tree}");
        git_in(dir.path(), &["repack", "-a"]);
        clear_pack_cache();
        let odb = Odb::new(&objects);
        let loose_path = tree_oid.loose_path_in(&objects);
        assert!(
            loose_path.is_file(),
            "loose copy must remain after repack -a"
        );
        let loose = Odb::read_loose_verify_oid(&loose_path, &tree_oid).expect("loose tree");
        let packed = odb.read(&tree_oid).expect("packed tree");
        assert_eq!(packed.data, loose.data);
        assert_eq!(packed.kind, loose.kind);
    }

    #[test]
    fn freshly_written_loose_object_visible_before_repack() {
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("seed.txt"), b"seed").unwrap();
        git_in(dir.path(), &["add", "seed.txt"]);
        git_in(dir.path(), &["commit", "-m", "seed"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let odb = Odb::new(&objects);
        let fresh = b"brand-new loose blob\n";
        let oid = odb.write(ObjectKind::Blob, fresh).expect("loose write");
        let obj = odb.read(&oid).expect("immediate loose read");
        assert_eq!(obj.data.as_slice(), fresh);
    }

    #[test]
    fn pack_added_after_first_read_found_via_odb_reprepare() {
        use crate::pack::{clear_pack_cache, pack_cache_test_guard};

        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("a.txt"), b"pack-a").unwrap();
        git_in(dir.path(), &["add", "a.txt"]);
        git_in(dir.path(), &["commit", "-m", "a"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let pack_dir = objects.join("pack");
        let git_dir = dir.path().join(".git");

        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        let warm_oid = git_rev_parse_oid(dir.path(), "HEAD");
        odb.read(&warm_oid).expect("warm read");

        fs::write(dir.path().join("b.txt"), b"pack-b").unwrap();
        git_in(dir.path(), &["add", "b.txt"]);
        git_in(dir.path(), &["commit", "-m", "b"]);
        let new_oid = git_rev_parse_oid(dir.path(), "HEAD");

        std::thread::sleep(std::time::Duration::from_secs(1));
        filetime::set_file_mtime(&pack_dir, filetime::FileTime::now()).unwrap();

        let obj = odb.read(&new_oid).expect("new pack via reprepare");
        assert_eq!(obj.kind, ObjectKind::Commit);
    }

    #[test]
    fn late_midx_after_warm_read_resolves_new_commit() {
        use crate::pack::{clear_pack_cache, pack_cache_test_guard};

        let _guard = pack_cache_test_guard();
        clear_pack_cache();
        let dir = TempDir::new().unwrap();
        git_in(dir.path(), &["init", "-q", "-b", "main"]);
        git_in(dir.path(), &["config", "user.email", "t@example.com"]);
        git_in(dir.path(), &["config", "user.name", "T"]);
        fs::write(dir.path().join("a.txt"), b"pack-a").unwrap();
        git_in(dir.path(), &["add", "a.txt"]);
        git_in(dir.path(), &["commit", "-m", "a"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        let objects = dir.path().join(".git/objects");
        let git_dir = dir.path().join(".git");
        assert!(
            crate::midx::cached_tip_midx_path(&objects.join("pack")).is_none(),
            "fixture starts without a MIDX"
        );

        let odb = Odb::new(&objects).with_config_git_dir(git_dir);
        let warm_oid = git_rev_parse_oid(dir.path(), "HEAD");
        odb.read(&warm_oid).expect("warm read before MIDX exists");

        fs::write(dir.path().join("b.txt"), b"pack-b").unwrap();
        git_in(dir.path(), &["add", "b.txt"]);
        git_in(dir.path(), &["commit", "-m", "b"]);
        git_in(dir.path(), &["repack", "-a", "-d"]);
        git_in(dir.path(), &["multi-pack-index", "write"]);
        let new_oid = git_rev_parse_oid(dir.path(), "HEAD");

        let cat = git_run(dir.path(), &["cat-file", "-e", &new_oid.to_hex()]);
        assert!(
            cat.status.success(),
            "system git must resolve the new commit"
        );

        let obj = odb
            .read(&new_oid)
            .expect("MIDX-covered commit after late MIDX write");
        assert_eq!(obj.kind, ObjectKind::Commit);
        assert!(
            crate::midx::cached_tip_midx_path(&objects.join("pack")).is_some(),
            "successful read after late MIDX must refresh the tip cache"
        );
    }
}
