//! Linked worktree admin scoped to a [`Repository`](crate::repo::Repository).

use std::path::{Path, PathBuf};

use crate::error::Result;
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;
use crate::worktree::{
    add_linked_worktree_with_cancel, ensure_branch_not_checked_out_elsewhere,
    ensure_branch_not_checked_out_in_any_worktree, find_worktree_with_branch_checked_out,
    list_worktrees, lock_linked_worktree, move_linked_worktree, prune_linked_worktrees,
    remove_linked_worktree, repair_worktrees, unlock_linked_worktree, worktree_admin_dir,
    AddLinkedWorktreeOptions, MoveWorktreeOptions, PruneWorktreesOptions, PrunedWorktreeEntry,
    WorktreeEntry, WorktreeRepair,
};

pub use crate::worktree::LockWorktreeOptions;

/// Worktree listing and linked-worktree mutations for one open repository.
#[derive(Debug, Copy, Clone)]
pub struct RepositoryWorktrees<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryWorktrees<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Enumerate the main and linked worktrees (main first, then linked by admin id).
    ///
    /// # Errors
    ///
    /// Propagates filesystem and HEAD resolution failures.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// let repo = Repository::discover(Some(Path::new(".")), &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let rows = repo.worktrees().list()?;
    /// let _ = rows.first().map(|e| &e.path);
    /// # Ok(())
    /// # }
    /// ```
    pub fn list(&self) -> Result<Vec<WorktreeEntry>> {
        list_worktrees(self.repo)
    }

    /// Register and populate a linked worktree at `options.path`.
    ///
    /// Populating the work tree reports
    /// [`ProgressPhase::CheckingOutFiles`](crate::progress::ProgressPhase::CheckingOutFiles)
    /// through `progress` and polls `cancel`. On cancel or any failure after registration
    /// begins, the new admin directory, a work tree directory this call created, and a newly
    /// created branch are removed again.
    ///
    /// # Errors
    ///
    /// Bare repository, occupied path and branch conflict failures,
    /// and [`crate::error::Error::Cancelled`] when `cancel` fires.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::progress::{CancelToken, NullProgress};
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::worktree::AddLinkedWorktreeOptions;
    /// # use std::path::PathBuf;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// # let repo = grit_lib::repo::Repository::discover(None, &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let opts = AddLinkedWorktreeOptions::new(PathBuf::from("/tmp/wt"))
    ///     .with_branch(Some(grit_lib::worktree::WorktreeBranch::Create("feature".into())));
    /// let _ = repo.worktrees().add(&opts, &mut NullProgress, &CancelToken::never());
    /// # Ok(())
    /// # }
    /// ```
    pub fn add(
        &self,
        options: &AddLinkedWorktreeOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<WorktreeEntry> {
        add_linked_worktree_with_cancel(self.repo, options, progress, cancel)
    }

    /// Admin directory for linked worktrees under this repository.
    #[must_use]
    pub fn admin_dir(&self) -> PathBuf {
        worktree_admin_dir(self.repo)
    }

    /// Path to a linked worktree that has `branch` checked out, if any.
    pub fn find_with_branch_checked_out(&self, branch: &str) -> Result<Option<PathBuf>> {
        Ok(
            find_worktree_with_branch_checked_out(self.repo, branch, None)?
                .map(|checkout| checkout.worktree_path),
        )
    }

    /// Refuse when `branch` is checked out in another linked worktree (not the main worktree).
    pub fn ensure_branch_not_checked_out_elsewhere(&self, branch: &str) -> Result<()> {
        ensure_branch_not_checked_out_elsewhere(self.repo, branch)
    }

    /// Refuse when `branch` is checked out in any worktree including the main one.
    pub fn ensure_branch_not_checked_out_in_any_worktree(&self, branch: &str) -> Result<()> {
        ensure_branch_not_checked_out_in_any_worktree(self.repo, branch)
    }

    /// Remove a linked worktree at `path`.
    ///
    /// Unless `force` is set, the worktree is first checked for local changes (a status
    /// scan that polls `cancel`); nothing is removed when that check is cancelled, and once
    /// deletion starts it runs to completion. No progress phases are reported yet.
    ///
    /// # Errors
    ///
    /// Propagates [`remove_linked_worktree`] failures, and [`crate::error::Error::Cancelled`]
    /// when `cancel` fires.
    pub fn remove(
        &self,
        path: &Path,
        force: bool,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()> {
        remove_linked_worktree(self.repo, path, force, progress, cancel)
    }

    /// Create the `locked` file for the linked worktree at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`LinkedWorktreeError::NotRegistered`](crate::worktree::LinkedWorktreeError::NotRegistered),
    /// [`LinkedWorktreeError::MainWorktree`](crate::worktree::LinkedWorktreeError::MainWorktree) or
    /// [`LinkedWorktreeError::AlreadyLocked`](crate::worktree::LinkedWorktreeError::AlreadyLocked), or an
    /// I/O error writing the file.
    ///
    /// # Examples
    ///
    /// ```
    /// # use grit_lib::repo::Repository;
    /// # use grit_lib::repo_handles::LockWorktreeOptions;
    /// # use std::path::Path;
    /// # fn demo() -> grit_lib::error::Result<()> {
    /// # let repo = grit_lib::repo::Repository::discover(None, &grit_lib::config::environment::RepositoryOptions::empty())?;
    /// let opts = LockWorktreeOptions::default().with_reason(Some("maintenance".into()));
    /// let _ = repo.worktrees().lock(Path::new("/tmp/wt"), &opts);
    /// # Ok(())
    /// # }
    /// ```
    pub fn lock(&self, path: &Path, options: &LockWorktreeOptions) -> Result<()> {
        lock_linked_worktree(self.repo, path, options.reason.as_deref())
    }

    /// Remove the `locked` file for the linked worktree at `path`.
    ///
    /// # Errors
    ///
    /// Returns [`LinkedWorktreeError::NotRegistered`](crate::worktree::LinkedWorktreeError::NotRegistered),
    /// [`LinkedWorktreeError::MainWorktree`](crate::worktree::LinkedWorktreeError::MainWorktree) or
    /// [`LinkedWorktreeError::NotLocked`](crate::worktree::LinkedWorktreeError::NotLocked), or an I/O error
    /// removing the file.
    pub fn unlock(&self, path: &Path) -> Result<()> {
        unlock_linked_worktree(self.repo, path)
    }

    /// Move a linked worktree to `to` and rewrite admin `gitdir` / worktree `.git` links.
    ///
    /// `cancel` is polled before the directory is moved; the move itself is a rename and is
    /// not interrupted. No progress phases are reported yet.
    ///
    /// # Errors
    ///
    /// Propagates [`move_linked_worktree`] failures, and [`crate::error::Error::Cancelled`]
    /// when `cancel` fires before the move.
    pub fn move_linked(
        &self,
        from: &Path,
        to: &Path,
        options: &MoveWorktreeOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()> {
        move_linked_worktree(self.repo, from, to, options, progress, cancel)
    }

    /// Repair `.git` / `gitdir` links for linked worktrees.
    ///
    /// # Errors
    ///
    /// Propagates [`repair_worktrees`] failures.
    pub fn repair(&self, paths: &[&Path], dry_run: bool) -> Result<Vec<WorktreeRepair>> {
        repair_worktrees(self.repo, paths, dry_run)
    }

    /// Remove stale linked worktree registrations with missing directories.
    pub fn prune(&self, options: &PruneWorktreesOptions) -> Result<Vec<PrunedWorktreeEntry>> {
        prune_linked_worktrees(self.repo, options)
    }
}
