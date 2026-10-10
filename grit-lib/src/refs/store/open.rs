//! Open the correct [`super::RefStore`] backend for a git directory.

use std::path::Path;
use std::sync::Arc;

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
pub fn open_ref_store(git_dir: &Path) -> Result<Arc<dyn RefStore>> {
    open_ref_store_uncached(git_dir, &Environment::empty())
}

/// Open a ref store without per-repository caching (used by [`RepoCaches`]).
pub(crate) fn open_ref_store_uncached(
    git_dir: &Path,
    env: &Environment,
) -> Result<Arc<dyn RefStore>> {
    let format = OnDiskFormat::detect(git_dir)?;
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
