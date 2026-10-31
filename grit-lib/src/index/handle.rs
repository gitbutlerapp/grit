//! Repository-scoped index load/write via [`IndexHandle`].
use std::path::Path;

use crate::error::Result;
use crate::index::Index;
use crate::repo::Repository;
use crate::split_index::WriteSplitIndexRequest;

/// Load and persist the Git index for one repository.
///
/// # Examples
///
/// ```
/// use grit_lib::repo::{InitOptions, Repository};
///
/// # fn main() -> grit_lib::error::Result<()> {
/// let dir = tempfile::tempdir()?;
/// let repo = Repository::init(dir.path(), &InitOptions::default())?;
/// let handle = repo.index_handle();
/// let index = handle.load()?;
/// assert!(index.entries().is_empty());
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Copy, Clone)]
pub struct IndexHandle<'repo> {
    repo: &'repo Repository,
}

impl<'repo> IndexHandle<'repo> {
    /// Bind index operations to `repo`.
    #[inline]
    #[must_use]
    pub fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[inline]
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Path to the index file this repository uses by default.
    #[inline]
    #[must_use]
    pub fn path(&self) -> std::path::PathBuf {
        self.repo.index_path()
    }

    /// Resolve the index path honoring `GIT_INDEX_FILE` when set.
    ///
    /// # Errors
    ///
    /// Propagates configuration errors.
    #[inline]
    pub fn path_for_env(&self) -> Result<std::path::PathBuf> {
        self.repo.index_path_for_env()
    }

    /// Load the index from the path selected by `GIT_INDEX_FILE` / the default `.git/index`.
    ///
    /// # Errors
    ///
    /// Propagates index parse, sparse, and split-index errors.
    #[inline]
    pub fn load(&self) -> Result<Index> {
        self.repo.load_index()
    }

    /// Load the index without expanding sparse-directory placeholders.
    ///
    /// Use for read-mostly porcelain (`status`, `diff`) on cone sparse-index checkouts.
    /// [`Self::load`] still expands every placeholder for callers that need a fully
    /// materialized index.
    #[inline]
    pub fn load_preserving_sparse(&self) -> Result<Index> {
        self.repo.load_index_preserving_sparse()
    }

    /// Load an index from an explicit on-disk path.
    #[inline]
    pub fn load_at(&self, path: &Path) -> Result<Index> {
        self.repo.load_index_at(path)
    }

    /// Write `index` to the index path selected by `GIT_INDEX_FILE` / the default `.git/index`.
    ///
    /// # Errors
    ///
    /// Propagates index serialization and I/O errors.
    #[inline]
    pub fn write(&self, index: &mut Index) -> Result<()> {
        self.repo.write_index(index)
    }

    /// Write `index` to an explicit path.
    #[inline]
    pub fn write_at(&self, path: &Path, index: &mut Index) -> Result<()> {
        self.repo.write_index_at(path, index)
    }

    /// Write `index` to `path` with explicit split-index emission control.
    #[inline]
    pub fn write_at_with_split(
        &self,
        path: &Path,
        index: &mut Index,
        split: WriteSplitIndexRequest,
    ) -> Result<()> {
        self.repo.write_index_at_with_split(path, index, split)
    }

    /// Acquire a lock on the index path selected by `GIT_INDEX_FILE` / the default `.git/index`.
    #[inline]
    pub fn begin_update(&self) -> Result<crate::index::IndexLock> {
        self.repo.begin_index_update()
    }

    /// Write `index` using a lock from [`Self::begin_update`].
    #[inline]
    pub fn commit_update(
        &self,
        lock: &mut crate::index::IndexLock,
        index: &mut Index,
        updated_workdir: bool,
    ) -> Result<()> {
        self.repo.commit_index_update(lock, index, updated_workdir)
    }

    /// Persist the index when the lock can be acquired (Git `repo_update_index_if_able`),
    /// using the path selected by `GIT_INDEX_FILE` when set.
    #[inline]
    pub fn try_write(&self, index: &mut Index) -> Result<bool> {
        self.repo.try_write_index(index)
    }
}
