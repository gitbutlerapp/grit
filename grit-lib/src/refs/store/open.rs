//! Open the correct [`super::RefStore`] backend for a git directory.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::environment::Environment;
use crate::error::Result;
use crate::ref_storage::RefStorageFormat as OnDiskFormat;

use super::routing::resolve_common_dir;
use super::{FilesRefStore, FilesRefStoreConfig, RefStore, ReftableRefStore};
use crate::refs::LogRefsConfig;

/// Select and open the ref store for `git_dir` (detects backend via [`OnDiskFormat::detect`]).
///
/// Prefer [`crate::repo_caches::RepoCaches::open_ref_store`] when a [`crate::repo::Repository`]
/// handle is available so the backend instance is reused for the lifetime of the repo.
///
/// # Errors
///
/// Propagates format detection and backend open failures.
// hygiene: process-wide ref store cache to avoid repeated commondir/config reads on hot paths
static PROCESS_REF_STORE: Mutex<Option<HashMap<PathBuf, Arc<dyn RefStore>>>> = Mutex::new(None);

/// Drop the process-wide [`open_ref_store`] cache (used when repository config changes).
pub(crate) fn clear_process_ref_store_cache() {
    if let Ok(mut guard) = PROCESS_REF_STORE.lock() {
        *guard = None;
    }
}

/// Open the ref store for `git_dir`, reusing a process-wide instance when possible.
///
/// # Errors
///
/// Propagates format detection and backend open failures.
pub fn open_ref_store(git_dir: &Path) -> Result<Arc<dyn RefStore>> {
    let key = git_dir
        .canonicalize()
        .unwrap_or_else(|_| git_dir.to_path_buf());
    if let Ok(mut guard) = PROCESS_REF_STORE.lock() {
        let map = guard.get_or_insert_with(HashMap::new);
        if let Some(store) = map.get(&key) {
            return Ok(Arc::clone(store));
        }
        let store = open_ref_store_uncached(git_dir, &Environment::empty(), None)?;
        map.insert(key, Arc::clone(&store));
        return Ok(store);
    }
    open_ref_store_uncached(git_dir, &Environment::empty(), None)
}

/// Open a ref store without per-repository caching (used by [`RepoCaches`]).
pub(crate) fn open_ref_store_uncached(
    git_dir: &Path,
    env: &Environment,
    caches: Option<&crate::repo_caches::RepoCaches>,
) -> Result<Arc<dyn RefStore>> {
    let format = if let Some(c) = caches {
        c.ref_storage_format_from_local_config(git_dir)?
    } else {
        OnDiskFormat::detect(git_dir)?
    };
    let store: Arc<dyn RefStore> = match format {
        OnDiskFormat::Files => {
            let git_dir_buf = std::path::PathBuf::from(git_dir);
            let git_dir_canon = std::fs::canonicalize(&git_dir_buf).unwrap_or(git_dir_buf);
            let common_dir = resolve_common_dir(&git_dir_canon);
            Arc::new(FilesRefStore::open(FilesRefStoreConfig {
                git_dir: git_dir_canon,
                common_dir,
                namespace_prefix: crate::ref_namespace::ref_storage_prefix(env),
                // Avoid loading full config while config conditionals may call `resolve_ref`.
                log_refs: LogRefsConfig::Unset,
            }))
        }
        OnDiskFormat::Reftable => Arc::new(ReftableRefStore::open(git_dir.to_path_buf())?),
    };
    Ok(store)
}
