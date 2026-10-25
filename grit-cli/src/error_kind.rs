//! Stable machine-readable categories for structured `--json` errors.

use grit_lib::porcelain::branch::BranchError;
use schemars::JsonSchema;
use serde::Serialize;

/// Machine-readable failure category in structured JSON errors (`kind` field).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorKind {
    /// Merge, pick, rebase, or similar would introduce conflicts.
    Conflict,
    /// Object ingestion detected an SHA-1 chosen-prefix collision (sha1dc).
    HashCollision,
    /// Checkout or switch would overwrite local or untracked files.
    WouldOverwrite,
    /// Refused because a merge, rebase, pick or revert is paused (finish or abort it first).
    OperationInProgress,
    /// Invalid `--author`, `--grep`, pickaxe, or similar filter pattern.
    InvalidPattern,
    /// Invalid command arguments for a specific subcommand (exit code 2).
    Usage,
    /// Incompatible flag combination (often with `--json` / `--markdown`).
    IncompatibleFlags,
    /// Another process holds the index lock; refs, reflog and index are unchanged, so retrying is safe.
    IndexLocked,
    /// Branch already exists on create.
    BranchAlreadyExists,
    /// Named branch does not exist.
    BranchNotFound,
    /// `grit switch -` when there is no previous branch in the reflog.
    NoPreviousBranch,
    /// Branch name failed validation.
    BranchInvalidName,
    /// Delete refused: branch is not fully merged.
    BranchNotFullyMerged,
    /// Branch is checked out in this or another worktree.
    BranchCheckedOut,
    /// Repository has no commits yet.
    BranchNoCommits,
    /// `--start` / tracking start point could not be resolved.
    BranchInvalidStartPoint,
    /// Upstream is not a tracking branch.
    BranchUpstreamNotTracking,
    /// Branch has no upstream configured.
    BranchNoUpstream,
    /// Other branch operation failure.
    BranchError,
    /// A hook exited non-zero (`hook.name`, `hook.status`, `hook.stdout`, `hook.stderr`).
    HookFailed,
    /// A branch, tag, commit id or revision expression names nothing.
    RevisionNotFound,
    /// Clone, fetch, pull, push, or `remote refs` could not reach or read the remote (exit 128).
    RemoteUnreadable,
    /// Push has no remote: none was named and no `origin` is configured.
    NoRemote,
    /// A named remote is not configured.
    RemoteNotFound,
    /// A configured remote has no URL.
    RemoteNoUrl,
    /// The server wants credentials and none were available.
    AuthRequired,
    /// The server rejected the credentials that were sent.
    AuthRejected,
    /// `merge --continue` found conflicts that are not resolved (or not staged) yet (`paths`).
    UnmergedPaths,
    /// Staged or unstaged changes block the operation (`paths`).
    DirtyWorktree,
    /// Worktree file I/O failed on a named path (`paths`, often one entry).
    WorktreePathIo,
}

impl ErrorKind {
    /// Map a library branch error to the JSON `kind` string.
    #[must_use]
    pub fn from_branch_error(err: &BranchError) -> Self {
        match err {
            BranchError::AlreadyExists { .. } => Self::BranchAlreadyExists,
            BranchError::NotFound { .. } => Self::BranchNotFound,
            BranchError::NoPreviousBranch => Self::NoPreviousBranch,
            BranchError::InvalidName { .. } => Self::BranchInvalidName,
            BranchError::NotFullyMerged { .. } => Self::BranchNotFullyMerged,
            BranchError::CurrentlyCheckedOut { .. } | BranchError::CheckedOut { .. } => {
                Self::BranchCheckedOut
            }
            BranchError::NoCommits => Self::BranchNoCommits,
            BranchError::InvalidStartPoint { .. } => Self::BranchInvalidStartPoint,
            BranchError::UpstreamNotTracking { .. } => Self::BranchUpstreamNotTracking,
            BranchError::NoUpstream { .. } => Self::BranchNoUpstream,
            _ => Self::BranchError,
        }
    }

    /// Snake-case slug documented on [Exit codes](https://grit-scm.com/docs/exit-codes).
    #[must_use]
    #[allow(dead_code)]
    pub fn doc_slug(self) -> &'static str {
        match self {
            Self::Conflict => "conflict",
            Self::HashCollision => "hash_collision",
            Self::WouldOverwrite => "would_overwrite",
            Self::OperationInProgress => "operation_in_progress",
            Self::InvalidPattern => "invalid_pattern",
            Self::Usage => "usage",
            Self::IncompatibleFlags => "incompatible_flags",
            Self::IndexLocked => "index_locked",
            Self::BranchAlreadyExists => "branch_already_exists",
            Self::BranchNotFound => "branch_not_found",
            Self::NoPreviousBranch => "no_previous_branch",
            Self::BranchInvalidName => "branch_invalid_name",
            Self::BranchNotFullyMerged => "branch_not_fully_merged",
            Self::BranchCheckedOut => "branch_checked_out",
            Self::BranchNoCommits => "branch_no_commits",
            Self::BranchInvalidStartPoint => "branch_invalid_start_point",
            Self::BranchUpstreamNotTracking => "branch_upstream_not_tracking",
            Self::BranchNoUpstream => "branch_no_upstream",
            Self::BranchError => "branch_error",
            Self::HookFailed => "hook_failed",
            Self::RevisionNotFound => "revision_not_found",
            Self::RemoteUnreadable => "remote_unreadable",
            Self::NoRemote => "no_remote",
            Self::RemoteNotFound => "remote_not_found",
            Self::RemoteNoUrl => "remote_no_url",
            Self::AuthRequired => "auth_required",
            Self::AuthRejected => "auth_rejected",
            Self::UnmergedPaths => "unmerged_paths",
            Self::DirtyWorktree => "dirty_worktree",
            Self::WorktreePathIo => "worktree_path_io",
        }
    }

    /// Every variant emitted today (for docs and contract tests).
    #[must_use]
    #[allow(dead_code)]
    pub fn all() -> &'static [Self] {
        &[
            Self::Conflict,
            Self::HashCollision,
            Self::WouldOverwrite,
            Self::OperationInProgress,
            Self::InvalidPattern,
            Self::Usage,
            Self::IncompatibleFlags,
            Self::IndexLocked,
            Self::BranchAlreadyExists,
            Self::BranchNotFound,
            Self::NoPreviousBranch,
            Self::BranchInvalidName,
            Self::BranchNotFullyMerged,
            Self::BranchCheckedOut,
            Self::BranchNoCommits,
            Self::BranchInvalidStartPoint,
            Self::BranchUpstreamNotTracking,
            Self::BranchNoUpstream,
            Self::BranchError,
            Self::HookFailed,
            Self::RevisionNotFound,
            Self::RemoteUnreadable,
            Self::NoRemote,
            Self::RemoteNotFound,
            Self::RemoteNoUrl,
            Self::AuthRequired,
            Self::AuthRejected,
            Self::UnmergedPaths,
            Self::DirtyWorktree,
            Self::WorktreePathIo,
        ]
    }
}
