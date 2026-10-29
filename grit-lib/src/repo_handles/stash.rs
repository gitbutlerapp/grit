//! Stash stack operations scoped to a [`Repository`](crate::repo::Repository).
//!
//! [`RepositoryStash`] (from [`Repository::stash`](crate::repo::Repository::stash)) is the one
//! public stash API; option and result types live in [`crate::stash`].

use crate::diff::{DiffEntry, DiffOptions, DiffStat};
use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::stash::{
    apply_stash_entry, apply_stash_oid, clear_stashes, create_stash, drop_stash, list_stashes,
    pop_stash_entry, preview_stash, push_stash, stash_branch, stash_diff, stash_diff_stat,
    StashApplyOptions, StashApplyOutcome, StashBranchOutcome, StashDiffOptions, StashEntry,
    StashPreview, StashPushOptions, StashPushResult,
};
use crate::progress::{Cancel, ProgressSink};
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
    /// Hashing local changes reports [`crate::progress::ProgressPhase::StagingFiles`] and the
    /// reset reports [`crate::progress::ProgressPhase::CheckingOutFiles`] to `progress`.
    ///
    /// # Errors
    ///
    /// - [`crate::error::Error::StashOptionConflict`] when [`StashPushOptions::dry_run`] is set
    ///   (use [`Self::preview`]).
    /// - [`crate::error::Error::StashNoInitialCommit`] before the first commit.
    /// - [`crate::error::Error::Cancelled`] when `cancel` or
    ///   [`ProgressSink::should_cancel`] fires. Before `refs/stash` moves, the repository is
    ///   unchanged; once the entry is stored it stays, even when the reset that follows is
    ///   cancelled (the same rule as a failed reset).
    /// - Stash build, ref, and checkout failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::config::ident_resolve::IdentRole;
    /// # use grit_lib::stash::{StashMode, StashPushOptions};
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let opts = StashPushOptions {
    ///     message: Some("wip".into()),
    ///     include_untracked: false,
    ///     identity: repo.identity(IdentRole::Committer, repo.identity_now())?,
    ///     selection: None,
    ///     pathspecs: Vec::new(),
    ///     dry_run: false,
    ///     mode: StashMode::default(),
    /// };
    /// let _ = repo.stash().push(
    ///     &opts,
    ///     &mut grit_lib::progress::NullProgress,
    ///     &grit_lib::progress::NeverCancel,
    /// )?;
    /// # Ok(())
    /// # }
    /// ```
    pub fn push(
        &self,
        options: &StashPushOptions,
        progress: &mut dyn ProgressSink,
        cancel: &dyn Cancel,
    ) -> Result<Option<StashPushResult>> {
        push_stash(self.repo, options, progress, cancel)
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
    /// The conflicted paths and their kinds; [`StashApplyOutcome::has_conflicts`] is false on a
    /// clean apply.
    ///
    /// Progress and cancellation work as in [`Self::apply_commit`].
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::StashNotFound`] when `index` is out of range, plus the
    /// [`Self::apply_commit`] errors.
    pub fn apply(
        &self,
        index: usize,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &dyn Cancel,
    ) -> Result<StashApplyOutcome> {
        apply_stash_entry(self.repo, index, options, progress, cancel)
    }

    /// Apply any stash-shaped commit (for example one from [`Self::create`]) without touching
    /// `refs/stash`.
    ///
    /// Bulk work-tree writes report [`crate::progress::ProgressPhase::CheckingOutFiles`] to
    /// `progress`; `cancel` is checked before and while the work tree is written.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::CorruptStash`] for a malformed stash commit, plus local-change,
    /// untracked-overwrite, and index write failures. [`crate::error::Error::Cancelled`] when
    /// cancelled: the index is not written, `refs/stash` is untouched, and the bulk checkout is
    /// rolled back; files content-merged onto a moved `HEAD` before the cancel stay written.
    pub fn apply_commit(
        &self,
        stash: &ObjectId,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &dyn Cancel,
    ) -> Result<StashApplyOutcome> {
        apply_stash_oid(self.repo, stash, options, progress, cancel)
    }

    /// Apply `stash@{index}` and drop it when it applied without conflicts.
    ///
    /// # Returns
    ///
    /// The conflicted paths and their kinds. When [`StashApplyOutcome::has_conflicts`] is true
    /// the entry is kept.
    ///
    /// A cancelled apply keeps the entry; see [`Self::apply_commit`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::apply`] and [`Self::drop`].
    pub fn pop(
        &self,
        index: usize,
        options: &StashApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &dyn Cancel,
    ) -> Result<StashApplyOutcome> {
        pop_stash_entry(self.repo, index, options, progress, cancel)
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

    /// Tree diff of `stash@{index}` against the commit it was made on (`git stash show`), in
    /// path order.
    ///
    /// # Errors
    ///
    /// [`crate::error::Error::StashNotFound`] or [`crate::error::Error::CorruptStash`] when the
    /// entry is missing or malformed.
    pub fn diff(&self, index: usize, options: &StashDiffOptions) -> Result<Vec<DiffEntry>> {
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
    /// let diff_options = DiffOptions::from_config(repo.config()?.as_ref());
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
    ) -> Result<DiffStat> {
        stash_diff_stat(self.repo, index, options, diff_options)
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
