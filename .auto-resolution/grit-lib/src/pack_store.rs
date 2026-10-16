//! Repository-scoped pack and MIDX read caches.
//!
//! [`PackStore`] holds parsed pack indexes, pack file bytes, directory listings (with mtime
//! signatures for reprepare-on-miss), multi-pack-index views, and the delta-base LRU. Each
//! `objects/` directory gets its own store; [`crate::odb::Odb`] owns an [`Arc<PackStore>`]
//! and installs it as thread-local context during [`Odb::read`](crate::odb::Odb::read).
//! After repack or index-pack, call [`Odb::invalidate_packs`](crate::odb::Odb::invalidate_packs)
//! on any handle for that repository so cached listings do not outlive removed pack files.

use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

/// Counters and hooks for pack-store integration tests.
pub mod test_hooks {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static LEGACY_STORES_CREATED: AtomicUsize = AtomicUsize::new(0);
    static IMPLICIT_STORES_CREATED: AtomicUsize = AtomicUsize::new(0);

    /// `reset` function.
    pub fn reset() {
        LEGACY_STORES_CREATED.store(0, Ordering::Release);
        IMPLICIT_STORES_CREATED.store(0, Ordering::Release);
    }

    /// `legacy_stores_created` function.
    pub fn legacy_stores_created() -> usize {
        LEGACY_STORES_CREATED.load(Ordering::Acquire)
    }

    /// `implicit_stores_created` function.
    pub fn implicit_stores_created() -> usize {
        IMPLICIT_STORES_CREATED.load(Ordering::Acquire)
    }

    pub(crate) fn record_legacy_store_created() {
        LEGACY_STORES_CREATED.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn record_implicit_store_created() {
        IMPLICIT_STORES_CREATED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Per-`objects/` cache of pack indexes, bytes, MIDX views, and delta-base LRU state.
///
/// [`crate::odb::Odb`] owns an [`Arc<PackStore>`]; [`Odb`](crate::odb::Odb) clones share it.
/// Independently opened [`Odb`](crate::odb::Odb) handles for the same path get separate stores.
/// Alternate object directories get separate stores retained on the parent [`Odb`](crate::odb::Odb).
pub struct PackStore {
    objects_dir: PathBuf,
    /// Bumped on every invalidation so other threads drop trusted thread-local pack caches.
    cache_epoch: AtomicU64,
    catalog: RwLock<crate::pack::pack_cache::CatalogState>,
    delta_bases: crate::pack::pack_cache::ShardedDeltaBaseLru,
    midx: Mutex<crate::midx::midx_cache::State>,
}

const _: () = {
    fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<PackStore>;
};

// hygiene: scoped read contexts and legacy standalone caches must not cross threads
thread_local! {
    static CURRENT: RefCell<Option<Arc<PackStore>>> = const { RefCell::new(None) };
    static ACTIVE_OBJECTS_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
    /// Standalone pack reads (no [`crate::odb::Odb`]) reuse one store per `objects/` on this thread.
    static LEGACY_BY_DIR: RefCell<HashMap<PathBuf, Arc<PackStore>>> = RefCell::new(HashMap::new());
    static IMPLICIT: RefCell<Option<Arc<PackStore>>> = const { RefCell::new(None) };
}

impl PackStore {
    /// Create a fresh store for `objects_dir` (not shared with other independent [`Odb`](crate::odb::Odb) opens).
    #[must_use]
    pub fn new(objects_dir: PathBuf) -> Self {
        Self {
            objects_dir,
            cache_epoch: AtomicU64::new(0),
            catalog: RwLock::new(crate::pack::pack_cache::CatalogState::default()),
            delta_bases: crate::pack::pack_cache::ShardedDeltaBaseLru::new(),
            midx: Mutex::new(crate::midx::midx_cache::State::default()),
        }
    }

    /// Generation counter incremented when pack caches are invalidated.
    #[must_use]
    pub(crate) fn cache_epoch(&self) -> u64 {
        self.cache_epoch.load(Ordering::Acquire)
    }

    pub(crate) fn bump_cache_epoch(&self) -> u64 {
        self.cache_epoch.fetch_add(1, Ordering::Release) + 1
    }

    /// Pack catalog (indexes, bytes, directory listings) for this store.
    pub(crate) fn with_catalog<R>(
        &self,
        f: impl FnOnce(&mut crate::pack::pack_cache::CatalogState) -> R,
    ) -> R {
        f(&mut self.catalog.write().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn with_catalog_read<R>(
        &self,
        f: impl FnOnce(&crate::pack::pack_cache::CatalogState) -> R,
    ) -> R {
        f(&self.catalog.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Like [`Self::with_catalog`], but skips `f` when the catalog write lock is contended.
    pub(crate) fn try_with_catalog<R>(
        &self,
        f: impl FnOnce(&mut crate::pack::pack_cache::CatalogState) -> R,
    ) -> Option<R> {
        let mut guard = self.catalog.try_write().ok()?;
        Some(f(&mut guard))
    }

    /// Sharded delta-base LRU (independent of the catalog mutex).
    #[must_use]
    pub(crate) fn delta_bases(&self) -> &crate::pack::pack_cache::ShardedDeltaBaseLru {
        &self.delta_bases
    }

    /// Resolve the [`PackStore`] used for pack-cache reads on this thread.
    pub(crate) fn active_for_reads() -> Arc<Self> {
        if let Some(store) = CURRENT.with(|c| c.borrow().clone()) {
            return store;
        }
        if let Some(dir) = ACTIVE_OBJECTS_DIR.with(|d| d.borrow().clone()) {
            return Self::legacy_for_objects_dir(&dir);
        }
        IMPLICIT.with(|i| {
            if let Some(s) = i.borrow().as_ref() {
                return Arc::clone(s);
            }
            test_hooks::record_implicit_store_created();
            let s = Arc::new(Self::new(PathBuf::from("__grit_implicit_pack_store")));
            *i.borrow_mut() = Some(Arc::clone(&s));
            s
        })
    }

    /// Legacy standalone reads: thread-local store for `objects_dir` (not shared with [`Odb`]).
    pub(crate) fn legacy_for_objects_dir(objects_dir: &Path) -> Arc<Self> {
        LEGACY_BY_DIR.with(|map| {
            let key = objects_dir.to_path_buf();
            if let Some(s) = map.borrow().get(&key) {
                return Arc::clone(s);
            }
            test_hooks::record_legacy_store_created();
            let store = Arc::new(Self::new(key.clone()));
            map.borrow_mut().insert(key, Arc::clone(&store));
            store
        })
    }

    /// Store for pack reads under a standard `…/objects/pack/*.pack` path (legacy API only).
    pub(crate) fn legacy_for_pack_path(pack_path: &Path) -> Arc<Self> {
        let objects_dir = pack_path.parent().and_then(|pack_dir| {
            (pack_dir.file_name() == Some(std::ffi::OsStr::new("pack")))
                .then(|| pack_dir.parent())
                .flatten()
        });
        match objects_dir {
            Some(dir) => Self::legacy_for_objects_dir(dir),
            None => IMPLICIT.with(|i| {
                if let Some(s) = i.borrow().as_ref() {
                    return Arc::clone(s);
                }
                test_hooks::record_implicit_store_created();
                let s = Arc::new(Self::new(PathBuf::from("__grit_nonstandard_pack_cache")));
                *i.borrow_mut() = Some(Arc::clone(&s));
                s
            }),
        }
    }

    /// `objects/` path for this store.
    #[must_use]
    pub fn objects_dir(&self) -> &Path {
        &self.objects_dir
    }

    /// Whether this thread already has an active pack-read store context.
    #[must_use]
    pub fn has_context() -> bool {
        CURRENT.with(|c| c.borrow().is_some())
    }

    /// Whether the active read context is scoped to `objects_dir`.
    #[must_use]
    pub(crate) fn read_context_matches_objects_dir(objects_dir: &Path) -> bool {
        Self::has_context()
            && ACTIVE_OBJECTS_DIR.with(|d| d.borrow().as_deref() == Some(objects_dir))
    }

    /// Run `f` with `store` as the pack-read context on this thread.
    pub fn with_context<R>(store: Arc<Self>, f: impl FnOnce() -> R) -> R {
        let _guard = PackReadContextGuard::install(store);
        f()
    }

    /// Drop cached pack listings, indexes, pack bytes, and delta bases.
    pub fn invalidate_packs(&self) {
        crate::pack::pack_cache::clear_catalog_on(&self.catalog);
        self.delta_bases.clear();
        let epoch = self.bump_cache_epoch();
        crate::pack::pack_cache::clear_thread_local_after_invalidation(epoch);
    }

    /// Drop MIDX bytes and tip cache under this store's `objects/pack/`.
    pub fn invalidate_midx(&self) {
        crate::midx::midx_cache::evict_pack_dir_on(&self.midx, &self.objects_dir.join("pack"));
    }

    /// Full pack + MIDX invalidation (after repack/gc/index-pack in-process).
    pub fn invalidate_all(&self) {
        self.invalidate_packs();
        self.invalidate_midx();
    }

    /// Apply `core.deltaBaseCacheLimit` from config to this store's delta-base LRU.
    pub fn configure_delta_base_from_config(&self, cfg: Option<&crate::config::ConfigSet>) {
        crate::pack::pack_cache::configure_delta_base_on(&self.delta_bases, cfg);
    }

    pub(crate) fn with_pack<R>(
        &self,
        f: impl FnOnce(&mut crate::pack::pack_cache::CatalogState) -> R,
    ) -> R {
        self.with_catalog(f)
    }

    pub(crate) fn with_midx<R>(
        &self,
        f: impl FnOnce(&mut crate::midx::midx_cache::State) -> R,
    ) -> R {
        f(&mut self.midx.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Run `f` on the store for the active read context (current store or legacy fallback).
    pub(crate) fn with_current_or_implicit<R>(
        f: impl FnOnce(&mut crate::pack::pack_cache::CatalogState) -> R,
    ) -> R {
        Self::active_for_reads().with_pack(f)
    }

    pub(crate) fn with_current_or_implicit_midx<R>(
        f: impl FnOnce(&mut crate::midx::midx_cache::State) -> R,
    ) -> R {
        if let Some(store) = CURRENT.with(|c| c.borrow().clone()) {
            return store.with_midx(f);
        }
        if let Some(dir) = ACTIVE_OBJECTS_DIR.with(|d| d.borrow().clone()) {
            return Self::legacy_for_objects_dir(&dir).with_midx(f);
        }
        Self::active_for_reads().with_midx(f)
    }

    /// Clear legacy thread-local stores (tests/benches calling [`crate::pack::clear_pack_cache`]).
    pub fn invalidate_all_legacy() {
        LEGACY_BY_DIR.with(|map| {
            for store in map.borrow().values() {
                store.invalidate_all();
            }
            map.borrow_mut().clear();
        });
        IMPLICIT.with(|i| *i.borrow_mut() = None);
    }

    /// Drop cached pack state for legacy standalone reads of `objects_dir`.
    pub(crate) fn invalidate_legacy_for_objects_dir(objects_dir: &Path) {
        LEGACY_BY_DIR.with(|map| {
            if let Some(store) = map.borrow().get(objects_dir) {
                store.invalidate_all();
            }
        });
    }
}

/// Keeps a thread-local pack read context alive for batch parallel reads.
pub(crate) struct PackReadContextGuard {
    prev_store: Option<Arc<PackStore>>,
    prev_dir: Option<PathBuf>,
}

impl PackReadContextGuard {
    pub(crate) fn install(store: Arc<PackStore>) -> Self {
        let prev_store = CURRENT.with(|c| c.borrow_mut().replace(Arc::clone(&store)));
        let prev_dir =
            ACTIVE_OBJECTS_DIR.with(|d| d.borrow_mut().replace(store.objects_dir().to_path_buf()));
        crate::pack::pack_cache::warm_thread_local_from_store(&store);
        Self {
            prev_store,
            prev_dir,
        }
    }
}

impl Drop for PackReadContextGuard {
    fn drop(&mut self) {
        crate::pack::clear_pack_read_info_cache();
        ACTIVE_OBJECTS_DIR.with(|d| *d.borrow_mut() = self.prev_dir.take());
        CURRENT.with(|c| *c.borrow_mut() = self.prev_store.take());
    }
}
