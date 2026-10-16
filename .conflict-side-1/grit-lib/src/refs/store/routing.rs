//! Per-ref storage directory routing (worktree, common dir, namespaces).

use std::path::{Path, PathBuf};

use crate::refs::common_dir;

/// Resolved locations for reading/writing one ref by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefStorageRoute {
    /// Directory containing `refs/` and `logs/` for this ref.
    pub storage_dir: PathBuf,
    /// On-disk ref path relative to `storage_dir` (namespace-aware).
    pub storage_name: String,
}

/// Compute storage directory and file name for `refname` in a linked checkout.
#[must_use]
#[allow(dead_code)]
pub fn route_ref_storage(git_dir: &Path, refname: &str) -> RefStorageRoute {
    route_ref_storage_with_namespace(git_dir, refname, None)
}

/// Like [`route_ref_storage`], but applies `namespace_prefix` from config instead of the environment.
#[must_use]
pub fn route_ref_storage_with_namespace(
    git_dir: &Path,
    refname: &str,
    namespace_prefix: Option<&str>,
) -> RefStorageRoute {
    let (storage_dir, stor_name) = crate::worktree_ref::resolve_ref_storage(git_dir, refname);
    let storage_name =
        crate::ref_namespace::storage_ref_name_with_prefix(namespace_prefix, &stor_name);
    RefStorageRoute {
        storage_dir,
        storage_name,
    }
}

/// Shared git directory (commondir link target), or `git_dir` when not linked.
#[must_use]
pub fn resolve_common_dir(git_dir: &Path) -> PathBuf {
    common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf())
}
