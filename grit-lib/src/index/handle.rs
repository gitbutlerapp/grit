//! Repository-scoped index load/write via [`IndexHandle`].
use std::path::Path;

use crate::error::Result;
use crate::index::Index;
use crate::repo::Repository;

/// Load and persist the Git index for one repository.
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

    /// Load the index from the path selected by `GIT_INDEX_FILE` / the default `.git/index`.
    ///
    /// # Errors
    ///
    /// Propagates index parse, sparse, and split-index errors.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::repo::init_repository;
    /// use grit_lib::RefStorageFormat;
    /// let dir = std::env::temp_dir().join("grit-index-load-doc");
    /// let repo = init_repository(&dir, false, "main", None, RefStorageFormat::Files)?;
    /// let index = repo.index_handle().load()?;
    /// assert!(index.entries().is_empty());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn load(&self) -> Result<Index> {
        self.repo.load_index()
    }

    /// Load an index from an explicit on-disk path.
    #[inline]
    pub fn load_at(&self, path: &Path) -> Result<Index> {
        self.repo.load_index_at(path)
    }

    /// Write `index` to the repository's default index path.
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
}
