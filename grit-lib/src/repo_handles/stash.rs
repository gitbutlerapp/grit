//! Stash stack operations scoped to a [`Repository`](crate::repo::Repository).
//!
//! [`RepositoryStash`] (from [`Repository::stash`](crate::repo::Repository::stash)) is the one
//! public stash API; option and result types live in [`crate::stash`].

use crate::diff::DiffEntry;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::stash::{
    apply_stash_entry, apply_stash_oid, clear_stashes, create_stash, drop_stash, list_stashes,
    pop_stash_entry, preview_stash, push_stash, stash_branch, stash_diff, StashApplyOptions,
    StashApplyOutcome, StashBranchOutcome, StashDiffOptions, StashEntry, StashPreview,
    StashPushOptions, StashPushResult,
};
use crate::repo::Repository;

/// Stash list/push/apply/pop/drop for one open repository.
///
/// Entries are addressed by their newest-first index (`0` is `stash@{0}`). Operations that touch
/// the worktree return [`crate::error::Error::StashBareRepository`] on a bare repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryStash<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryStash<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// List stash entries from `refs/stash` reflog (newest first).
    ///
    /// # Errors
    ///
    /// Propagates reflog read failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let entries = repo.stash().list()?;
    /// let _ = entries.len();
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self) -> Result<Vec<StashEntry>> {
        list_stashes(self.repo)
    }

    /// Save local changes as a new `stash@{0}` and reset the saved paths to `HEAD`.
    ///
    /// Returns `None` when there is nothing to stash. Untracked paths included in the stash
    /// are removed from the worktree.
    ///
    /// # Errors
    ///
    /// - [`crate::error::Error::StashOptionConflict`] when [`StashPushOptions::dry_run`] is set
    ///   (use [`Self::preview`]).
    /// - [`crate::error::Error::StashNoInitialCommit`] before the first commit.
    /// - Stash build, ref, and checkout failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::stash::{StashMode, StashPushOptions};
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let opts = StashPushOptions {
    ///     message: Some("wip".into()),
    ///     include_untracked: false,
    ///     identity: "User <u@example.com> 1000000000 +0000".into(),
    ///     selection: None,
    ///     pathspecs: Vec::new(),
    ///     dry_run: false,
    ///     mode: StashMode::default(),
    /// };
    /// let _ = repo.stash().push(&opts)?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn push(&self, options: &StashPushOptions) -> Result<Option<StashPushResult>> {
        push_stash(self.repo, options)
    }

    /// Report which paths [`Self::push`] would save, without writing objects, refs, or files.
    ///
    /// Returns `None` when there is nothing to stash.
    ///
    /// # Errors
    ///
    /// Same validation and read failures as [`Self::push`].
    pub fn preview(&self, options: &StashPushOptions) -> Result<Option<StashPreview>> {
        preview_stash(self.repo, options)
    }

    /// Build a stash commit without updating `refs/stash` or touching the worktree.
    ///
    /// Returns `None` when there is nothing to stash.
    ///
    /// # Errors
    ///
    /// Same build failures as [`Self::push`].
    pub fn create(&self, options: &StashPushOptions) -> Result<Option<ObjectId>> {
        create_stash(self.repo, options)
    }

    /// Apply `stash@{index}` without dropping it.
    ///
    /// # Returns
    ///
    /// The conflicted paths; [`StashApplyOutcome::has_conflicts`] is false on a clean apply.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::StashNotFound`] when `index` is out of range, plus the
    /// [`Self::apply_commit`] errors.
    pub fn apply(&self, index: usize, options: &StashApplyOptions) -> Result<StashApplyOutcome> {
        apply_stash_entry(self.repo, index, options)
    }

    /// Apply any stash-shaped commit (for example one from [`Self::create`]) without touching
    /// `refs/stash`.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptStash`] for a malformed stash commit, plus local-change,
    /// untracked-overwrite, and index write failures.
    pub fn apply_commit(
        &self,
        stash: &ObjectId,
        options: &StashApplyOptions,
    ) -> Result<StashApplyOutcome> {
        apply_stash_oid(self.repo, stash, options)
    }

    /// Apply `stash@{index}` and drop it when it applied without conflicts.
    ///
    /// # Returns
    ///
    /// The conflicted paths. When [`StashApplyOutcome::has_conflicts`] is true the entry is kept.
    ///
    /// # Errors
    ///
    /// Same as [`Self::apply`] and [`Self::drop`].
    pub fn pop(&self, index: usize, options: &StashApplyOptions) -> Result<StashApplyOutcome> {
        pop_stash_entry(self.repo, index, options)
    }

    /// Remove `stash@{index}`, moving `refs/stash` to the next entry (or deleting it).
    ///
    /// # Errors
    ///
    /// Returns [`crate::error::Error::StashNotFound`] when `index` is out of range.
    pub fn drop(&self, index: usize) -> Result<()> {
        drop_stash(self.repo, index)
    }

    /// Remove every stash entry, returning how many were removed.
    ///
    /// # Errors
    ///
    /// Propagates ref store and reflog delete failures.
    pub fn clear(&self) -> Result<usize> {
        clear_stashes(self.repo)
    }

    /// Tree diff of `stash@{index}` against the commit it was made on (`git stash show`).
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::StashNotFound`] or [`crate::error::Error::CorruptStash`] when the
    /// entry is missing or malformed.
    pub fn diff(&self, index: usize, options: &StashDiffOptions) -> Result<Vec<DiffEntry>> {
        stash_diff(self.repo, index, options)
    }

    /// Create branch `name` at the commit `stash@{index}` was made on, switch to it, apply the
    /// stash with its index, and drop the entry when that applied cleanly.
    ///
    /// # Errors
    ///
    /// [`crate::porcelain::branch_error::BranchError::AlreadyExists`] when `name` is taken,
    /// [`crate::error::Error::StashNotFound`] when `index` is out of range, and switch or apply
    /// failures otherwise.
    pub fn branch(&self, name: &str, index: usize) -> Result<StashBranchOutcome> {
        stash_branch(self.repo, name, index)
    }
}
