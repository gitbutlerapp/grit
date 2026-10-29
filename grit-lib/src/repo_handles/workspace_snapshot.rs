//! Workspace snapshot capture/restore for undo logs.

use crate::error::Result;
use crate::porcelain::workspace_snapshot::{
    capture_workspace_snapshot, restore_workspace_snapshot, WorkspaceCaptureOptions,
    WorkspaceRestoreOptions, WorkspaceSnapshot,
};
use crate::progress::{Cancel, ProgressSink};
use crate::repo::Repository;

/// Capture and restore full workspace state (index stages, worktree, refs, operation sentinels).
#[derive(Debug, Copy, Clone)]
pub struct RepositoryWorkspaceSnapshot<'repo> {
    repo: &'repo Repository,
}

impl<'repo> RepositoryWorkspaceSnapshot<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Capture the current workspace without mutating the repository.
    ///
    /// # Errors
    ///
    /// Propagates errors from [`capture_workspace_snapshot`].
    pub fn capture(&self, options: &WorkspaceCaptureOptions) -> Result<WorkspaceSnapshot> {
        capture_workspace_snapshot(self.repo, options)
    }

    /// Restore a snapshot produced by [`Self::capture`].
    ///
    /// # Errors
    ///
    /// Propagates errors from [`restore_workspace_snapshot`].
    pub fn restore(
        &self,
        snapshot: &WorkspaceSnapshot,
        options: &WorkspaceRestoreOptions,
        progress: &mut dyn ProgressSink,
        cancel: &dyn Cancel,
    ) -> Result<()> {
        restore_workspace_snapshot(self.repo, snapshot, options, progress, cancel)
    }
}
