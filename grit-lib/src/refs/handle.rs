//! Repository-scoped reference and reflog access via [`References`].
use std::ops::ControlFlow;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::listing::{query_refs, RefListing, RefQuery, RefQueryError, RefQueryOptions};
use crate::refs::store::{Expected, RawRef, RefName, RefUpdate, RefUpdateFlags};
use crate::refs::{
    commit_ref_update_on_store, ensure_refname_safe_for_storage, list_refs_for_repository,
    update_direct_ref_on_store,
};
use crate::repo::Repository;

/// A ref returned by [`References::read_existing`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExistingRef {
    /// Value as stored (a direct oid or a symbolic target).
    pub(crate) raw: RawRef,
    /// Object the ref resolves to.
    pub(crate) oid: ObjectId,
}

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
<<<<<<< New base: Merge branch 'factory/commit-dry-run-3068'
    ///     ..Default::default()
||||||| Common ancestor
=======
    ///     staged_change_count: None,
    ///     hooks: Default::default(),
>>>>>>> Current commit: commit: message file, author override, and no-verify hooks
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

    /// Read `name` when it exists: its stored value and the object it resolves to.
    ///
    /// Unlike [`resolve`](Self::resolve), a missing ref is `Ok(None)` rather than an error,
    /// so read and corruption failures stay distinguishable from absence. A direct ref is
    /// read once; only a symbolic ref is followed.
    pub(crate) fn read_existing(&self, name: &str) -> Result<Option<ExistingRef>> {
        let Some(raw) = self.repo.refs().read_raw(name)? else {
            return Ok(None);
        };
        let oid = match &raw {
            RawRef::Direct(oid) => *oid,
            RawRef::Symbolic(_) => self.repo.resolve_ref_name(name)?,
        };
        Ok(Some(ExistingRef { raw, oid }))
    }

    /// Resolve `name` when it exists, `Ok(None)` when it does not (see [`Self::read_existing`]).
    pub(crate) fn resolve_if_exists(&self, name: &str) -> Result<Option<ObjectId>> {
        Ok(self.read_existing(name)?.map(|existing| existing.oid))
    }

    /// Delete `name` and its reflog through this repository's ref store, provided it still
    /// has the value `existing` was read with.
    ///
    /// The delete is a compare-and-swap, so a concurrent update between the caller's checks
    /// and the delete fails instead of being lost. A symbolic ref is deleted itself, not its
    /// target.
    pub(crate) fn delete_verified(&self, name: &str, existing: &ExistingRef) -> Result<()> {
        ensure_refname_safe_for_storage(name)?;
        let (expected, no_deref) = match &existing.raw {
            RawRef::Symbolic(target) => (Expected::Symref(target.clone()), true),
            RawRef::Direct(oid) => (Expected::Oid(*oid), false),
        };
        let store = self.repo.refs();
        commit_ref_update_on_store(
            store,
            RefUpdate {
                name: name.to_owned(),
                new_value: None,
                expected,
                reflog: None,
                flags: RefUpdateFlags {
                    no_deref,
                    ..RefUpdateFlags::default()
                },
            },
        )?;
        store.delete_reflog(name)
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

    /// List refs matching `query` (patterns, sort, limit) through this repository's ref store.
    ///
    /// # Errors
    ///
    /// Returns [`RefQueryError`] when the ref store cannot be read, when creator-date sort
    /// is requested without object data, or when an object read fails.
    pub fn query(
        &self,
        query: &RefQuery,
        options: &RefQueryOptions,
    ) -> std::result::Result<Vec<RefListing>, RefQueryError> {
        query_refs(self.repo, query, options)
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
<<<<<<< New base: Merge branch 'factory/commit-dry-run-3068'
    ///     ..Default::default()
||||||| Common ancestor
=======
    ///     staged_change_count: None,
    ///     hooks: Default::default(),
>>>>>>> Current commit: commit: message file, author override, and no-verify hooks
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
