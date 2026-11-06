//! Repository-scoped reference and reflog access via [`References`].
use std::ops::ControlFlow;

use crate::config::ident_resolve::IdentRole;
use crate::error::{Error, Result};
use crate::lockfile::LockWait;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::listing::{query_refs, RefListing, RefQuery, RefQueryError, RefQueryOptions};
use crate::refs::on_store::reflog_wanted;
use crate::refs::store::{
    Expected, RawRef, RefName, RefNameRef, RefUpdate, RefUpdateFlags, ReflogUpdate,
};
use crate::refs::{
    commit_ref_update_on_store, commit_ref_updates_on_store_with_wait,
    ensure_refname_safe_for_storage, list_refs_for_repository,
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

    /// Low-level ref storage backend (same as [`Repository::ref_store`]).
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::refs::store::RefNameRef;
    /// use grit_lib::repo::{InitOptions, Repository};
    /// let dir = tempfile::tempdir()?;
    /// let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;
    /// let _ = repo
    ///     .references()
    ///     .store()
    ///     .read_raw(RefNameRef::from_trusted("HEAD"))?;
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    #[must_use]
    pub fn store(&self) -> &'repo dyn crate::refs::store::RefStore {
        self.repo.ref_store()
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
    /// use grit_lib::refs::store::{RefName, RefNameRef};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::ident::Signature;
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-refs-resolve-doc");
    /// let (repo, _) = Repository::init(&dir, &InitOptions::default())?;
    /// let mut req = CommitRequest::new("doc");
    /// req.author = Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?);
    /// req.committer = Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?);
    /// req.allow_empty = true;
    /// let _ = create_commit(&repo, &req, &mut NullProgress, &grit_lib::progress::CancelToken::never())?;
    /// let main = RefName::try_from("refs/heads/main").expect("valid ref");
    /// let head = repo.references().resolve(&main)?;
    /// assert!(!head.to_hex().is_empty());
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn resolve(&self, name: impl AsRef<RefNameRef>) -> Result<ObjectId> {
        self.repo.resolve_ref_oid(name.as_ref().as_str())
    }

    /// Read `name` when it exists: its stored value and the object it resolves to.
    ///
    /// Unlike [`resolve`](Self::resolve), a missing ref is `Ok(None)` rather than an error,
    /// so read and corruption failures stay distinguishable from absence. A direct ref is
    /// read once; only a symbolic ref is followed.
    pub(crate) fn read_existing(&self, name: &str) -> Result<Option<ExistingRef>> {
        let name = RefNameRef::from_trusted(name);
        let Some(raw) = self.repo.ref_store().read_raw(name)? else {
            return Ok(None);
        };
        let oid = match &raw {
            RawRef::Direct(oid) => *oid,
            RawRef::Symbolic(_) => self.repo.resolve_ref_oid(name.as_str())?,
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
        let name_ref = RefNameRef::from_trusted(name);
        let (expected, no_deref) = match &existing.raw {
            RawRef::Symbolic(target) => (
                Expected::Symref(RefName::from_trusted(target.clone())),
                true,
            ),
            RawRef::Direct(oid) => (Expected::Oid(*oid), false),
        };
        let store = self.store();
        commit_ref_update_on_store(
            store,
            RefUpdate {
                name: RefName::from_trusted(name.to_owned()),
                new_value: None,
                expected,
                reflog: None,
                flags: RefUpdateFlags {
                    no_deref,
                    ..RefUpdateFlags::default()
                },
            },
        )?;
        store.delete_reflog(name_ref)
    }

    /// List refs under `prefix` (for example `"refs/heads/"`), sorted by name.
    ///
    /// # Errors
    ///
    /// Propagates ref store iteration errors.
    #[inline]
    pub fn list(&self, prefix: &str) -> Result<Vec<(RefName, ObjectId)>> {
        Ok(list_refs_for_repository(self.repo, prefix)?
            .into_iter()
            .map(|(name, oid)| (RefName::from_trusted(name), oid))
            .collect())
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
    /// Symbolic refs are followed: updating `HEAD` while it points at `refs/heads/main` moves
    /// `main` and leaves `HEAD` symbolic. Reflog entries are appended when
    /// [`core.logAllRefUpdates`](crate::refs::LogRefsConfig) allows (same policy as
    /// `git update-ref`). Identity comes from the repository committer signature.
    ///
    /// To rewrite `HEAD` itself (detach or replace the symref file), use
    /// [`Self::update_direct`]. To require the ref tip to match an expected oid first, use
    /// [`Self::compare_and_swap`].
    ///
    /// `oid` must be stored in this repository, and must be a commit when `name` is a branch
    /// (`refs/heads/…`) or `HEAD`. This catches a commit that a concurrent prune deleted after
    /// you wrote it, instead of leaving the ref pointing at a missing object.
    ///
    /// # Errors
    ///
    /// - [`RefStoreError::MissingObject`](crate::refs::store::RefStoreError::MissingObject)
    ///   when the repository does not store `oid`.
    /// - [`RefStoreError::NotACommit`](crate::refs::store::RefStoreError::NotACommit) when
    ///   `name` is a branch or `HEAD` and `oid` is not a commit.
    /// - Validation, locking, and I/O errors from the ref backend.
    ///
    /// The two object errors are returned as [`crate::error::Error::RefStore`]. The ref is
    /// unchanged on error.
    ///
    /// # Examples
    ///
    /// ```
    /// # fn main() -> grit_lib::error::Result<()> {
    /// use grit_lib::refs::store::{RefName, RefNameRef};
    /// use grit_lib::repo::{InitOptions, Repository};
    /// use grit_lib::ident::Signature;
    /// use grit_lib::porcelain::commit::{create_commit, CommitRequest};
    /// use grit_lib::progress::NullProgress;
    /// let dir = std::env::temp_dir().join("grit-refs-write-doc");
    /// let (repo, _) = Repository::init(&dir, &InitOptions::default())?;
    /// let mut req = CommitRequest::new("doc");
    /// req.author = Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?);
    /// req.committer = Some(Signature::new("Doc", "doc@example.com", repo.identity_now())?);
    /// req.allow_empty = true;
    /// let commit = create_commit(&repo, &req, &mut NullProgress, &grit_lib::progress::CancelToken::never())?.oid;
    /// let name = RefName::try_from("refs/heads/handle-demo").expect("valid ref");
    /// repo.references().update(&name, &commit)?;
    /// assert_eq!(repo.references().resolve(&name)?, commit);
    /// # Ok(())
    /// # }
    /// ```
    #[inline]
    pub fn update(&self, name: &RefName, oid: &ObjectId) -> Result<()> {
        self.commit_point(name, oid, Expected::Any, RefUpdateFlags::default())
    }

    /// Point `name` at `oid` without following symbolic refs.
    ///
    /// Updating `HEAD` writes the object id into `.git/HEAD` (detached HEAD) instead of moving
    /// the branch `HEAD` points at. Reflog policy matches [`Self::update`].
    ///
    /// # Errors
    ///
    /// Same validation and backend errors as [`Self::update`].
    pub fn update_direct(&self, name: &RefName, oid: &ObjectId) -> Result<()> {
        self.commit_point(
            name,
            oid,
            Expected::Any,
            RefUpdateFlags {
                no_deref: true,
                ..RefUpdateFlags::default()
            },
        )
    }

    /// Point `name` at `oid` only when it currently resolves to `expected`.
    ///
    /// Symbolic refs are followed the same way as [`Self::update`]. On mismatch the ref is
    /// unchanged and the error is
    /// [`RefStoreError::ExpectedMismatch`](crate::refs::store::RefStoreError::ExpectedMismatch).
    ///
    /// # Errors
    ///
    /// Same as [`Self::update`], plus compare-and-swap mismatch.
    pub fn compare_and_swap(
        &self,
        name: &RefName,
        expected: &ObjectId,
        oid: &ObjectId,
    ) -> Result<()> {
        self.commit_point(
            name,
            oid,
            Expected::Oid(*expected),
            RefUpdateFlags::default(),
        )
    }

    fn commit_point(
        &self,
        name: &RefName,
        oid: &ObjectId,
        expected: Expected,
        flags: RefUpdateFlags,
    ) -> Result<()> {
        let refname = name.as_str();
        ensure_refname_safe_for_storage(refname)?;
        let store = self.store();
        let config = self.repo.config().snapshot()?;
        let signature = match self
            .repo
            .identity(IdentRole::Committer, self.repo.identity_now())
        {
            Ok(sig) => Some(sig),
            Err(Error::Identity(_)) => None,
            Err(err) => return Err(err),
        };
        let reflog_for = |target: &str| -> Result<Option<ReflogUpdate>> {
            let Some(sig) = signature.as_ref() else {
                return Ok(None);
            };
            Ok(reflog_wanted(store, config.as_ref(), target)?
                .then(|| ReflogUpdate::from_signature(sig, "")))
        };

        let old_tip = self.resolve_if_exists(refname)?;
        let mut updates = Vec::with_capacity(2);

        // Peeling `HEAD` must not attach one reflog decision to both `HEAD` and the target
        // branch: Git evaluates existing logs independently when log-all-ref-updates is off.
        let peeled_head_target = if !flags.no_deref && refname == "HEAD" {
            match store.read_raw(RefNameRef::from_trusted("HEAD"))? {
                Some(RawRef::Symbolic(target)) => Some(target),
                _ => None,
            }
        } else {
            None
        };

        if let Some(branch) = peeled_head_target {
            let branch_for_reflog = branch.clone();
            updates.push(RefUpdate {
                name: RefName::from_trusted(branch),
                new_value: Some(RawRef::Direct(*oid)),
                expected,
                reflog: reflog_for(branch_for_reflog.as_str())?,
                flags: RefUpdateFlags::default(),
            });
            if let Some(mut head_reflog) = reflog_for("HEAD")? {
                head_reflog.old_oid = old_tip;
                head_reflog.new_oid = Some(*oid);
                updates.push(RefUpdate {
                    name: RefName::from_trusted("HEAD"),
                    new_value: Some(RawRef::Direct(*oid)),
                    expected: Expected::Any,
                    reflog: Some(head_reflog),
                    flags: RefUpdateFlags {
                        log_only: true,
                        no_deref: true,
                        ..RefUpdateFlags::default()
                    },
                });
            }
        } else {
            updates.push(RefUpdate {
                name: RefName::from_trusted(refname.to_owned()),
                new_value: Some(RawRef::Direct(*oid)),
                expected,
                reflog: reflog_for(refname)?,
                flags,
            });

            let mirror_head = !flags.no_deref
                && refname.starts_with("refs/heads/")
                && matches!(
                    store.read_raw(RefNameRef::from_trusted("HEAD"))?,
                    Some(RawRef::Symbolic(head_branch)) if head_branch.as_str() == refname
                );
            if mirror_head {
                if let Some(mut head_reflog) = reflog_for("HEAD")? {
                    head_reflog.old_oid = old_tip;
                    head_reflog.new_oid = Some(*oid);
                    updates.push(RefUpdate {
                        name: RefName::from_trusted("HEAD"),
                        new_value: Some(RawRef::Direct(*oid)),
                        expected: Expected::Any,
                        reflog: Some(head_reflog),
                        flags: RefUpdateFlags {
                            log_only: true,
                            no_deref: true,
                            ..RefUpdateFlags::default()
                        },
                    });
                }
            }
        }

        commit_ref_updates_on_store_with_wait(store, &updates, || {
            LockWait::files_ref_lock(Some(config.as_ref()))
        })
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
            .ref_store()
            .for_each_reflog_entry(name.as_ref(), false, &mut |entry| {
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
