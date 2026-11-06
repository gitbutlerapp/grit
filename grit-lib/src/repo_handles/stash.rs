//! Stash stack operations scoped to a [`Repository`](crate::repo::Repository).
//!
//! [`RepositoryStash`] (from [`Repository::stash`](crate::repo::Repository::stash)) is the one
//! public stash API; option and result types live in [`crate::stash`].

use crate::diff::{DiffEntry, DiffOptions, DiffStat};
use crate::objects::ObjectId;
use crate::porcelain::stash::{
    apply_stash_entry, apply_stash_oid, apply_stash_reference, clear_stashes, create_stash,
    drop_stash, list_stashes, parse_stash_reference, pop_stash_entry, pop_stash_reference,
    preview_stash, push_stash, stash_branch, stash_branch_reference, stash_diff, stash_diff_stat,
    stash_index_for_oid, StashApplyOptions, StashApplyOutcome, StashBranchOutcome,
    StashDiffOptions, StashEntry, StashPreview, StashPushOptions, StashPushResult, StashReference,
};
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;
use crate::stash::StashResult;

/// Stash list/push/apply/pop/drop for one open repository.
///
/// Entries are addressed by their newest-first index (`0` is `stash@{0}`). Operations that touch
/// the worktree return [`crate::stash::StashError::BareRepository`] on a bare repository.
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
    pub fn list(&self) -> StashResult<Vec<StashEntry>> {
        list_stashes(self.repo)
    }

    /// Save local changes as a new `stash@{0}` and reset the saved paths to `HEAD`.
    ///
    /// Returns `None` when there is nothing to stash. Untracked paths included in the stash
    /// are removed from the worktree.
    ///
    /// Hashing local changes reports [`crate::progress::ProgressPhase::StagingFiles`] and the
    /// reset reports [`crate::progress::ProgressPhase::CheckingOutFiles`] to `progress`.
    ///
    /// # Errors
    ///
    /// - [`crate::stash::StashPushValidationError::DryRunUsePreview`] when
    ///   [`StashPushOptions::dry_run`] is set (use [`Self::preview`]).
    /// - [`crate::stash::StashError::NoInitialCommit`] before the first commit.
    /// - [`crate::stash::StashError::Cancelled`] when [`crate::progress::CancelToken::is_cancelled`]
    ///   is true on the `cancel` argument. Before `refs/stash` moves, the repository is
    ///   unchanged; once the entry is stored it stays, even when the reset that follows is
    ///   cancelled (the same rule as a failed reset).
    /// - Stash build, ref, and checkout failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::config::ident_resolve::IdentRole;
    /// # use grit_lib::stash::StashPushOptions;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let mut opts = StashPushOptions::new(
    ///     repo.identity(IdentRole::Committer, repo.identity_now())?,
    /// );
    /// opts.message = Some("wip".into());
    /// let _ = repo.stash().push(
    ///     &opts,
    ///     &mut grit_lib::progress::NullProgress,
    ///     &grit_lib::progress::CancelToken::never(),
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn push(
        &self,
        options: &StashPushOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> StashResult<Option<StashPushResult>> {
        push_stash(self.repo, options, progress, cancel)
    }

    /// Report which paths [`Self::push`] would save, without writing objects, refs, or files.
    pub fn preview(&self, options: &StashPushOptions) -> StashResult<Option<StashPreview>> {
        preview_stash(self.repo, options)
    }

    /// Build a stash commit without updating `refs/stash` or touching the worktree.
    pub fn create(&self, options: &StashPushOptions) -> StashResult<Option<ObjectId>> {
        create_stash(self.repo, options)
    }

    /// Apply `stash@{index}` without dropping it.
    pub fn apply(
        &self,
        index: usize,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> StashResult<StashApplyOutcome> {
        apply_stash_entry(self.repo, index, options, progress, cancel)
    }

    /// Apply any stash-shaped commit without touching `refs/stash`.
    pub fn apply_commit(
        &self,
        stash: &ObjectId,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> StashResult<StashApplyOutcome> {
        apply_stash_oid(self.repo, stash, options, progress, cancel)
    }

    /// Apply `stash@{index}` and drop it when it applied without conflicts.
    pub fn pop(
        &self,
        index: usize,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> StashResult<StashApplyOutcome> {
        pop_stash_entry(self.repo, index, options, progress, cancel)
    }

    /// Remove `stash@{index}`, moving `refs/stash` to the next entry (or deleting it).
    pub fn drop(&self, index: usize) -> StashResult<ObjectId> {
        drop_stash(self.repo, index)
    }

    /// Parse a stash selector (`stash@{{n}}`, decimal index, or commit id).
    pub fn parse_reference(&self, spec: Option<&str>) -> crate::error::Result<StashReference> {
        parse_stash_reference(self.repo, spec)
    }

    /// Newest-first index of a stash W commit on `refs/stash`, if present.
    #[must_use]
    pub fn index_for_oid(&self, oid: &ObjectId) -> Option<usize> {
        stash_index_for_oid(self.repo, oid)
    }

    /// Apply a parsed stash reference without dropping it.
    pub fn apply_reference(
        &self,
        reference: StashReference,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> StashResult<StashApplyOutcome> {
        apply_stash_reference(self.repo, reference, options, progress, cancel)
    }

    /// Pop a parsed stash reference; commit ids must still be on `refs/stash`.
    pub fn pop_reference(
        &self,
        reference: StashReference,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> StashResult<StashApplyOutcome> {
        pop_stash_reference(self.repo, reference, options, progress, cancel)
    }

    /// Create a branch from a parsed stash reference (on- or off-stack).
    pub fn branch_reference(
        &self,
        name: &str,
        reference: StashReference,
    ) -> StashResult<StashBranchOutcome> {
        stash_branch_reference(self.repo, name, reference)
    }

    /// Remove every stash entry, returning how many were removed.
    pub fn clear(&self) -> StashResult<usize> {
        clear_stashes(self.repo)
    }

    /// Tree diff of `stash@{index}` against the commit it was made on.
    pub fn diff(&self, index: usize, options: &StashDiffOptions) -> StashResult<Vec<DiffEntry>> {
        stash_diff(self.repo, index, options)
    }

    /// Per-file line counts of [`Self::diff`] (`git stash show --stat`).
    ///
    /// Counts come from the line-count engine, without building hunks. Files whose every
    /// change `diff_options.whitespace` ignores are left out, as Git does (see
    /// [`ListedFileDiff::changes_all_ignored`](crate::diff::ListedFileDiff::changes_all_ignored)).
    ///
    /// # Parameters
    ///
    /// - `index`: newest-first stash index (`0` is `stash@{0}`).
    /// - `options`: which stash parents to include.
    /// - `diff_options`: line-diff settings, e.g. [`DiffOptions::from_config`].
    ///
    /// # Errors
    ///
    /// The [`Self::diff`] errors, plus config load and object read failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::diff::DiffOptions;
    /// # use grit_lib::stash::StashDiffOptions;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let diff_options = DiffOptions::from_config(repo.config().snapshot()?.as_ref());
    /// let stat = repo.stash().diff_stat(0, &StashDiffOptions::default(), diff_options)?;
    /// let _ = stat.totals().insertions;
    /// # Ok(())
    /// # }
    /// ```
    pub fn diff_stat(
        &self,
        index: usize,
        options: &StashDiffOptions,
        diff_options: DiffOptions,
    ) -> StashResult<DiffStat> {
        stash_diff_stat(self.repo, index, options, diff_options)
    }

    /// Create branch `name` at the stash base, switch, apply with index, drop when clean.
    pub fn branch(&self, name: &str, index: usize) -> StashResult<StashBranchOutcome> {
        stash_branch(self.repo, name, index)
    }
}
