//! Switch, checkout, reset, restore, clean, and path moves for one repository.

use crate::diff::merge_file::ConflictStyle;
use crate::error::Result;
use crate::objects::ObjectId;
use crate::porcelain::branch::TrackMode;
use crate::porcelain::checkout::{
    checkout_between_trees_with_progress, checkout_hard_to_tree_from_head_tree,
    checkout_tree_changes, CheckoutOutcome,
};
use crate::porcelain::clean::{clean_untracked_with_cancel, CleanOptions, CleanOutcome};
use crate::porcelain::paths::{
    move_path, move_paths, remove_paths, MoveOptions, MoveOutcome, RemoveOptions, RemoveOutcome,
};
use crate::porcelain::reset::{
    preview_reset, reset_to_commit, ResetOptions, ResetOutcome, ResetPreview,
};
use crate::porcelain::restore::{
    conflict_style_from_config, restore_paths, RestoreOptions, RestoreOutcome,
};
use crate::porcelain::switch::{
    resolve_create_branch, resolve_revision_spec, resolve_switch_target, switch_repository,
    SwitchMode, SwitchOptions, SwitchOutcome,
};
use crate::progress::{poll_cancel, CancelToken, ProgressSink};
use crate::repo::Repository;

/// Checkout, switch, reset, restore, clean, and worktree path maintenance for one repository.
#[derive(Debug, Copy, Clone)]
pub struct Checkout<'repo> {
    repo: &'repo Repository,
}

impl<'repo> Checkout<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Resolve how `switch` would treat `spec`.
    pub fn resolve_switch_target(&self, spec: &str) -> Result<SwitchMode> {
        resolve_switch_target(self.repo, spec)
    }

    /// Peel `spec` to a commit oid for detached checkout or branch starts.
    pub fn resolve_revision_spec(&self, spec: &str) -> Result<ObjectId> {
        resolve_revision_spec(self.repo, spec)
    }

    /// Resolve branch creation fields for `switch --create`.
    pub fn resolve_create_branch(
        &self,
        name: &str,
        start: Option<&str>,
        track: TrackMode,
    ) -> Result<SwitchMode> {
        resolve_create_branch(self.repo, name, start, track)
    }

    /// Switch branches or detach `HEAD`, updating the working tree when needed.
    pub fn switch(
        &self,
        opts: &SwitchOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<SwitchOutcome> {
        switch_repository(self.repo, opts, progress, cancel)
    }

    /// Align the index and tracked worktree with tree `to`, optionally diffing from `from`.
    pub fn checkout(
        &self,
        from: Option<&ObjectId>,
        to: &ObjectId,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CheckoutOutcome> {
        checkout_between_trees_with_progress(self.repo, from, to, progress, cancel)
    }

    /// Hard-reset checkout from `head_tree` to `target_tree` (used by hard reset).
    pub fn checkout_hard_to_tree(
        &self,
        head_tree: &ObjectId,
        target_tree: &ObjectId,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<()> {
        checkout_hard_to_tree_from_head_tree(self.repo, head_tree, target_tree, progress, cancel)
    }

    /// Apply a set of tree/index path changes to the worktree.
    pub fn checkout_tree_changes(&self, changes: &[crate::diff::DiffEntry]) -> Result<()> {
        checkout_tree_changes(self.repo, changes)
    }

    /// Preview a reset without touching refs, index, or worktree.
    ///
    /// `cancel` is polled before and after the plan is computed. No progress phases are
    /// reported yet; `progress` is accepted for future use.
    ///
    /// # Errors
    ///
    /// The validation failures of [`Self::reset`], and [`crate::error::Error::Cancelled`]
    /// when `cancel` fires.
    pub fn preview_reset(
        &self,
        opts: &ResetOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<ResetPreview> {
        poll_cancel(progress, cancel)?;
        let preview = preview_reset(self.repo, opts)?;
        poll_cancel(progress, cancel)?;
        Ok(preview)
    }

    /// Move `HEAD` and update index/worktree per `opts`.
    pub fn reset(
        &self,
        opts: &ResetOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<ResetOutcome> {
        reset_to_commit(self.repo, opts, progress, cancel)
    }

    /// Restore paths from the index or another tree into the worktree.
    pub fn restore(
        &self,
        opts: &RestoreOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<RestoreOutcome> {
        restore_paths(self.repo, opts, progress, cancel)
    }

    /// Remove untracked paths from the worktree.
    pub fn clean(
        &self,
        opts: &CleanOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<CleanOutcome> {
        clean_untracked_with_cancel(self.repo, opts, progress, cancel)
    }

    /// Remove tracked paths from the index and worktree.
    ///
    /// `cancel` is polled before anything changes and while paths are collected. No
    /// progress phases are reported yet; `progress` is accepted for future use.
    ///
    /// # Errors
    ///
    /// Pathspec, blocked-removal, index and I/O failures, and
    /// [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn remove_paths(
        &self,
        opts: &RemoveOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<RemoveOutcome> {
        poll_cancel(progress, cancel)?;
        remove_paths(self.repo, opts, cancel)
    }

    /// Rename one tracked path in the index and worktree.
    ///
    /// Same as [`Self::move_paths`] with one source.
    ///
    /// # Errors
    ///
    /// Same as [`Self::move_paths`].
    pub fn move_path(
        &self,
        src: &str,
        dst: &str,
        force: bool,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<MoveOutcome> {
        poll_cancel(progress, cancel)?;
        move_path(self.repo, src, dst, force, cancel)
    }

    /// Move tracked sources into `opts.destination`, updating the index once.
    ///
    /// `cancel` is polled before anything changes and while the moves are planned. No
    /// progress phases are reported yet; `progress` is accepted for future use.
    ///
    /// # Errors
    ///
    /// Path validation, move conflicts, index and I/O failures, and
    /// [`crate::error::Error::Cancelled`] when `cancel` fires.
    pub fn move_paths(
        &self,
        opts: &MoveOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> Result<MoveOutcome> {
        poll_cancel(progress, cancel)?;
        move_paths(self.repo, opts, cancel)
    }

    /// Merge conflict style from repository config (`merge.conflictStyle` / `diff3`).
    #[must_use]
    pub fn conflict_style(&self) -> ConflictStyle {
        conflict_style_from_config(self.repo)
    }
}
