//! Repository-scoped reference and reflog access via [`References`].
use std::path::Path;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::reflog::{read_reflog, ReflogEntry};
use crate::refs::store::RefName;
use crate::refs::{list_refs_for_repository, write_ref};
use crate::repo::Repository;

/// Resolve, list, and update refs for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct References<'repo> {
    repo: &'repo Repository,
}

impl<'repo> References<'repo> {
    /// Bind ref operations to `repo`.
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

    /// Resolve a ref name to an object id, following symbolic refs.
    ///
    /// # Errors
    ///
    /// Propagates ref store and resolution errors.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::refs::store::RefName;
    /// use grit_lib::repo::init_repository;
    /// use grit_lib::RefStorageFormat;
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-refs-resolve-doc");
    /// let repo = init_repository(&dir, false, "main", None, RefStorageFormat::Files)?;
    /// let req = CommitRequest {
    ///     message: "doc".into(),
    ///     author: "Doc <doc@example.com>".into(),
    ///     committer: "Doc <doc@example.com>".into(),
    ///     allow_empty: true,
    ///     sign_override: None,
    ///     amend: false,
    /// };
    /// let _ = create_commit(&repo, &req, &mut NullProgress)?;
    /// let main = RefName::try_from("refs/heads/main").expect("valid ref");
    /// let head = repo.references().resolve(&main)?;
    /// assert!(!head.to_hex().is_empty());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn resolve(&self, name: &RefName) -> Result<ObjectId> {
        self.repo.resolve_ref_name(name.as_str())
    }

    /// List refs under `prefix` (for example `"refs/heads/"`), sorted by name.
    ///
    /// # Errors
    ///
    /// Propagates ref store iteration errors.
    #[inline]
    pub fn list(&self, prefix: &str) -> Result<Vec<(String, ObjectId)>> {
        list_refs_for_repository(self.repo, prefix)
    }

    /// Point `name` at `oid`, creating parent directories as needed.
    ///
    /// # Errors
    ///
    /// Propagates validation, locking, and I/O errors from the ref backend.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::refs::store::RefName;
    /// use grit_lib::repo::init_repository;
    /// use grit_lib::RefStorageFormat;
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-refs-write-doc");
    /// let repo = init_repository(&dir, false, "main", None, RefStorageFormat::Files)?;
    /// let req = CommitRequest {
    ///     message: "doc".into(),
    ///     author: "Doc <doc@example.com>".into(),
    ///     committer: "Doc <doc@example.com>".into(),
    ///     allow_empty: true,
    ///     sign_override: None,
    ///     amend: false,
    /// };
    /// let commit = create_commit(&repo, &req, &mut NullProgress)?.oid;
    /// let name = RefName::try_from("refs/heads/handle-demo").expect("valid ref");
    /// repo.references().update(&name, &commit)?;
    /// assert_eq!(repo.references().resolve(&name)?, commit);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn update(&self, name: &RefName, oid: &ObjectId) -> Result<()> {
        write_ref(self.git_dir(), name.as_str(), oid)
    }

    /// Read reflog entries for `name` (oldest first).
    ///
    /// # Errors
    ///
    /// Propagates reflog parse and I/O errors.
    #[inline]
    pub fn reflog(&self, name: &RefName) -> Result<Vec<ReflogEntry>> {
        read_reflog(self.git_dir(), name.as_str())
    }

    #[inline]
    fn git_dir(&self) -> &Path {
        &self.repo.git_dir
    }
}
