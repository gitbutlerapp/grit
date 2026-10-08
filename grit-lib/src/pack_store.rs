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
use std::sync::{Arc, Mutex};

/// Per-`objects/` cache of pack indexes, bytes, MIDX views, and delta-base LRU state.
///
/// [`crate::odb::Odb`] owns an [`Arc<PackStore>`]; [`Odb`](crate::odb::Odb) clones share it.
/// Independently opened [`Odb`](crate::odb::Odb) handles for the same path get separate stores.
/// Alternate object directories get separate stores retained on the parent [`Odb`](crate::odb::Odb).
pub struct PackStore {
    objects_dir: PathBuf,
    pack: Mutex<crate::pack::pack_cache::State>,
    midx: Mutex<crate::midx::midx_cache::State>,
}

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
            pack: Mutex::new(crate::pack::pack_cache::State::default()),
            midx: Mutex::new(crate::midx::midx_cache::State::default()),
        }
    }

    /// Legacy standalone reads: thread-local store for `objects_dir` (not shared with [`Odb`]).
    pub(crate) fn legacy_for_objects_dir(objects_dir: &Path) -> Arc<Self> {
        LEGACY_BY_DIR.with(|map| {
            let key = objects_dir.to_path_buf();
            if let Some(s) = map.borrow().get(&key) {
                return Arc::clone(s);
            }
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

    /// Run `f` with `store` as the pack-read context on this thread.
    pub fn with_context<R>(store: Arc<Self>, f: impl FnOnce() -> R) -> R {
        let _guard = PackReadContextGuard::install(store);
        f()
    }

    /// Drop cached pack listings, indexes, pack bytes, and delta bases.
    pub fn invalidate_packs(&self) {
        crate::pack::pack_cache::clear_on(&self.pack);
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
        crate::pack::pack_cache::configure_delta_base_on(&self.pack, cfg);
    }

    pub(crate) fn with_pack<R>(
        &self,
        f: impl FnOnce(&mut crate::pack::pack_cache::State) -> R,
    ) -> R {
        f(&mut self.pack.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub(crate) fn with_midx<R>(
        &self,
        f: impl FnOnce(&mut crate::midx::midx_cache::State) -> R,
    ) -> R {
        f(&mut self.midx.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// Run `f` on the store for the active read context (current store or legacy fallback).
    pub(crate) fn with_current_or_implicit<R>(
        f: impl FnOnce(&mut crate::pack::pack_cache::State) -> R,
    ) -> R {
        if let Some(store) = CURRENT.with(|c| c.borrow().clone()) {
            return store.with_pack(f);
        }
        if let Some(dir) = ACTIVE_OBJECTS_DIR.with(|d| d.borrow().clone()) {
            return Self::legacy_for_objects_dir(&dir).with_pack(f);
        }
        let store = IMPLICIT.with(|i| i.borrow().clone()).unwrap_or_else(|| {
            let s = Arc::new(Self::new(PathBuf::from("__grit_implicit_pack_store")));
            IMPLICIT.with(|i| *i.borrow_mut() = Some(Arc::clone(&s)));
            s
        });
        store.with_pack(f)
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
        let store = IMPLICIT.with(|i| i.borrow().clone()).unwrap_or_else(|| {
            let s = Arc::new(Self::new(PathBuf::from("__grit_implicit_pack_store")));
            IMPLICIT.with(|i| *i.borrow_mut() = Some(Arc::clone(&s)));
            s
        });
        store.with_midx(f)
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
}

struct PackReadContextGuard {
    prev_store: Option<Arc<PackStore>>,
    prev_dir: Option<PathBuf>,
}

impl PackReadContextGuard {
    fn install(store: Arc<PackStore>) -> Self {
        let prev_store = CURRENT.with(|c| c.borrow_mut().replace(Arc::clone(&store)));
        let prev_dir =
            ACTIVE_OBJECTS_DIR.with(|d| d.borrow_mut().replace(store.objects_dir().to_path_buf()));
        Self {
            prev_store,
            prev_dir,
        }
    }
}

impl Drop for PackReadContextGuard {
    fn drop(&mut self) {
        ACTIVE_OBJECTS_DIR.with(|d| *d.borrow_mut() = self.prev_dir.take());
        CURRENT.with(|c| *c.borrow_mut() = self.prev_store.take());
    }
}
