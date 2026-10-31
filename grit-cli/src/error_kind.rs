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
    /// A lock file such as `.git/index.lock` or a ref lock already exists (another process, or a
    /// stale lock); the lock is left alone (`lock.path`, `lock.holder`).
    LockHeld,
    /// Deprecated alias of `lock_held`, emitted by v0.5.21 for a held index lock. grit no
    /// longer emits it; it stays in the error schema for one release so v0.5.21 output still
    /// validates, and will be removed after that.
    #[allow(dead_code)]
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
    /// A checkout stopped part way and could not put some worktree paths back (`paths`);
    /// `HEAD` and the index are unchanged.
    CheckoutIncomplete,
    /// `grit clone` fetched every object and set up the refs, but checking out the work
    /// tree failed (for example a partial clone could not fetch its blobs). The repository
    /// is kept (`clone_checkout`).
    CloneCheckoutFailed,
    /// Git directory I/O failed on a named path under `.git/` (`paths`, often one entry).
    GitDirPathIo,
    /// Index-only commit while the work tree has unstaged or untracked changes.
    NothingStaged,
    /// Genuinely nothing to commit (clean tree or auto-stage cancel-out).
    NothingToCommit,
    /// Nested git checkout has no commit on its current branch (`paths`).
    EmbeddedRepoWithoutCommit,
    /// `HEAD` is on a GitButler workspace, which GitButler rewrites; commit with `but commit`.
    #[serde(rename = "gitbutler_workspace")]
    GitButlerWorkspace,
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

    /// Every variant emitted today (for contract tests).
    #[cfg(test)]
    #[must_use]
    pub fn all() -> &'static [Self] {
        &[
            Self::Conflict,
            Self::HashCollision,
            Self::WouldOverwrite,
            Self::OperationInProgress,
            Self::InvalidPattern,
            Self::Usage,
            Self::IncompatibleFlags,
            Self::LockHeld,
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
            Self::CheckoutIncomplete,
            Self::CloneCheckoutFailed,
            Self::GitDirPathIo,
            Self::NothingStaged,
            Self::NothingToCommit,
            Self::EmbeddedRepoWithoutCommit,
            Self::GitButlerWorkspace,
        ]
    }
}
