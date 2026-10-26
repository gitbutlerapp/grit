//! Open the correct [`super::RefStore`] backend for a git directory.

use std::path::Path;
use std::sync::Arc;

use crate::environment::Environment;
use crate::error::Result;
use crate::ref_storage::RefStorageFormat as OnDiskFormat;

use super::routing::resolve_common_dir;
use super::{FilesRefStore, FilesRefStoreConfig, RefStore, ReftableRefStore};

/// Select and open the ref store for `git_dir` (detects backend via [`OnDiskFormat::detect`]).
///
/// Prefer [`crate::repo_caches::RepoCaches::open_ref_store`] when a [`crate::repo::Repository`]
/// handle is available so the backend instance is reused for the lifetime of the repo.
///
/// # Errors
///
/// Propagates format detection and backend open failures.
pub fn open_ref_store(git_dir: &Path) -> Result<Arc<dyn RefStore>> {
    open_ref_store_uncached(git_dir, &Environment::empty(), None)
}

/// Initialize a fresh repository or linked worktree HEAD using its configured backend.
///
/// The git directory and (for a linked worktree) its `commondir` must already exist.
/// Reftable's loose files only support discovery and prevent legacy loose-ref writes.
/// Ref values are committed through the same transaction path as later updates.
pub(crate) fn initialize_head(git_dir: &Path, head: super::RawRef) -> Result<()> {
    let store = open_ref_store(git_dir)?;
    if store.format() == super::RefStorageFormat::Reftable {
        std::fs::create_dir_all(git_dir.join("reftable"))?;
        let tables = git_dir.join("reftable/tables.list");
        if !tables.exists() {
            std::fs::write(tables, "")?;
        }
        std::fs::create_dir_all(git_dir.join("refs"))?;
        let heads = git_dir.join("refs/heads");
        if heads.is_dir() {
            std::fs::remove_dir(&heads)?;
        }
        std::fs::write(heads, "this repository uses the reftable format\n")?;
        std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/.invalid\n")?;
    }
    let txn = super::RefTransaction::new().update(super::RefUpdate {
        name: "HEAD".to_owned(),
        new_value: Some(head),
        expected: super::Expected::Any,
        reflog: None,
        flags: super::RefUpdateFlags {
            no_deref: true,
            log_only: false,
        },
    })?;
    store.prepare(txn)?.commit()?;
    Ok(())
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
        OnDiskFormat::Files => Arc::new(files_ref_store(git_dir, env)),
        OnDiskFormat::Reftable => Arc::new(ReftableRefStore::open(git_dir.to_path_buf())?),
    };
    Ok(store)
}

/// The files backend [`open_ref_store`] opens for `git_dir`, or `None` when the repository
/// stores refs in reftable.
///
/// # Errors
///
/// Propagates ref storage format detection failures.
pub(crate) fn open_files_ref_store(git_dir: &Path) -> Result<Option<FilesRefStore>> {
    Ok(match OnDiskFormat::detect(git_dir)? {
        OnDiskFormat::Files => Some(files_ref_store(git_dir, &Environment::empty())),
        OnDiskFormat::Reftable => None,
    })
}

fn files_ref_store(git_dir: &Path, env: &Environment) -> FilesRefStore {
    let git_dir_buf = std::path::PathBuf::from(git_dir);
    let git_dir_canon = std::fs::canonicalize(&git_dir_buf).unwrap_or(git_dir_buf);
    let common_dir = resolve_common_dir(&git_dir_canon);
    FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir_canon,
        common_dir,
        namespace_prefix: crate::ref_namespace::ref_storage_prefix(env),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::store::RefStorageFormat;
    use filetime::FileTime;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn open_ref_store_rereads_when_config_content_changes_under_same_stamp() {
        let tmp = TempDir::new().expect("tempdir");
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).expect("git dir");
        let config = git_dir.join("config");
        const FILES_CONFIG: &str =
            "[core]\n\trepositoryformatversion = 0\n; pad-for-same-length-test.........\n";
        const REFTABLE_CONFIG: &str =
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\trefStorage = reftable\n";
        assert_eq!(FILES_CONFIG.len(), REFTABLE_CONFIG.len());
        fs::write(&config, FILES_CONFIG).expect("config v0");
        let before = open_ref_store(&git_dir).expect("open files");
        assert_eq!(before.format(), RefStorageFormat::Files);

        let t0 = FileTime::from_last_modification_time(&fs::metadata(&config).expect("stat"));
        fs::write(&config, REFTABLE_CONFIG).expect("rewrite reftable");
        filetime::set_file_mtime(&config, t0).expect("restore mtime");

        assert_eq!(
            OnDiskFormat::detect(&git_dir).expect("detect"),
            OnDiskFormat::Reftable
        );
        let after = open_ref_store(&git_dir).expect("open after rewrite");
        assert_eq!(
            after.format(),
            RefStorageFormat::Reftable,
            "must not reuse files store when config content changed under restored mtime"
        );
    }
}
