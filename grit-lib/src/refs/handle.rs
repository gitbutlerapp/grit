//! Repository-scoped reference and reflog access via [`References`].
use std::ops::ControlFlow;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::store::RefName;
use crate::refs::{list_refs_for_repository, update_direct_ref_on_store};
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
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-refs-resolve-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let req = CommitRequest {
    ///     message: "doc".into(),
    ///     author: "Doc <doc@example.com>".into(),
    ///     committer: "Doc <doc@example.com>".into(),
    ///     allow_empty: true,
    ///     sign_override: None,
    ///     amend: false,
    ///     assume_worktree_synced: false,
    ///     skip_pre_commit_tree_match: false,
    ///     staged_change_count: None,
    ///     hooks: Default::default(),
    ///     ..Default::default()
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
    /// Uses the [`RefStore`](crate::refs::store::RefStore) installed on this repository (including
    /// injected in-memory stores), not a fresh open from `git_dir` alone.
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
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-refs-write-doc");
    /// let repo = Repository::init(&dir, &InitOptions::default())?;
    /// let req = CommitRequest {
    ///     message: "doc".into(),
    ///     author: "Doc <doc@example.com>".into(),
    ///     committer: "Doc <doc@example.com>".into(),
    ///     allow_empty: true,
    ///     sign_override: None,
    ///     amend: false,
    ///     assume_worktree_synced: false,
    ///     skip_pre_commit_tree_match: false,
    ///     staged_change_count: None,
    ///     hooks: Default::default(),
    ///     ..Default::default()
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
        update_direct_ref_on_store(self.repo.refs(), self.repo.git_dir(), name.as_str(), oid)
    }

    /// Read reflog entries for `name` (oldest first).
    ///
    /// # Errors
    ///
    /// Propagates reflog parse and backend errors.
    #[inline]
    pub fn reflog(&self, name: &RefName) -> Result<Vec<ReflogEntry>> {
        let mut entries = Vec::new();
        self.repo
            .refs()
            .for_each_reflog_entry(name.as_str(), false, &mut |entry| {
                entries.push(entry.clone());
                ControlFlow::Continue(())
            })?;
        Ok(entries)
    }

    /// Pack loose refs into `packed-refs` (files backend) or compact reftable tables.
    ///
    /// # Errors
    ///
    /// Propagates ref-store, locking, and I/O failures.
    pub fn pack(
        &self,
        opts: &crate::maintenance::PackRefsOptions,
    ) -> Result<crate::maintenance::PackRefsReport> {
        crate::maintenance::pack_refs(self.repo, opts)
    }
}
