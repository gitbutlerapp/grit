//! Typed failures for branch create, delete, and upstream configuration.

use thiserror::Error;

use crate::objects::ObjectId;

/// Branch create, delete, and upstream failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum BranchError {
    /// `refs/heads/<name>` already exists and `force` was not set.
    #[error("branch '{name}' already exists")]
    AlreadyExists {
        /// Short branch name.
        name: String,
    },

    /// `refs/heads/<name>` does not exist.
    #[error("no branch named '{name}'")]
    NotFound {
        /// Short branch name.
        name: String,
    },

    /// The branch name failed ref-format validation.
    #[error("invalid branch name '{name}': {reason}")]
    InvalidName {
        /// User-supplied short name.
        name: String,
        /// Human-readable ref-format validation detail.
        reason: String,
    },

    /// Safe delete found commits not reachable from the merge base.
    #[error("the branch '{name}' is not fully merged into '{base}'")]
    NotFullyMerged {
        /// Short branch name.
        name: String,
        /// Display name of the merge base (upstream or `HEAD`).
        base: String,
    },

    /// The branch is checked out in a worktree (including the current branch on `HEAD`).
    #[error("branch '{name}' is already checked out in '{worktree}'")]
    CheckedOut {
        /// Short branch name.
        name: String,
        /// Worktree path that holds the branch.
        worktree: String,
    },

    /// No commit exists to create a branch from.
    #[error("no commits yet to create a branch from")]
    NoCommits,

    /// Explicit [`StartPoint`] oid is missing or not a commit.
    #[error("'{label}' is not a valid branch start point: {reason}")]
    InvalidStartPoint {
        /// Reflog label for the rejected start point.
        label: String,
        /// Why the oid was rejected (missing object, wrong type, etc.).
        reason: String,
    },

    /// [`crate::porcelain::branch::set_upstream`] was given a ref that is not a local or remote-tracking branch.
    #[error("upstream '{upstream}' is not a tracking branch")]
    UpstreamNotTracking {
        /// User-supplied upstream string.
        upstream: String,
    },

    /// [`crate::porcelain::branch::unset_upstream`] was called when no upstream is configured.
    #[error("branch '{name}' has no upstream configured")]
    NoUpstream {
        /// Short branch name.
        name: String,
    },
}

/// Start commit and reflog label for [`crate::porcelain::branch::CreateBranchOptions::start`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartPoint {
    /// Tip commit for the new or updated branch.
    pub oid: ObjectId,
    /// Label for `branch: Created from …` / `branch: Reset to …`.
    pub label: String,
}

/// Whether creating a branch should configure `branch.<name>.remote` / `.merge`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum TrackMode {
    /// Follow `branch.autoSetupMerge` and fetch refspec mapping.
    #[default]
    Auto,
    /// Always configure upstream when the start point allows it.
    Always,
    /// Never configure upstream.
    Never,
}

/// Safe delete requires the tip to be merged; force skips the check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeleteMode {
    /// Refuse when the tip is not reachable from upstream or `HEAD`.
    Safe,
    /// Delete regardless of merge state.
    Force,
}
