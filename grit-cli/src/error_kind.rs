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
    /// A branch, tag, commit id or revision expression names nothing.
    RevisionNotFound,
}

impl ErrorKind {
    /// Map a library branch error to the JSON `kind` string.
    #[must_use]
    pub fn from_branch_error(err: &BranchError) -> Self {
        match err {
            BranchError::AlreadyExists { .. } => Self::BranchAlreadyExists,
            BranchError::NotFound { .. } => Self::BranchNotFound,
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
            Self::InvalidPattern => "invalid_pattern",
            Self::Usage => "usage",
            Self::IncompatibleFlags => "incompatible_flags",
            Self::IndexLocked => "index_locked",
            Self::BranchAlreadyExists => "branch_already_exists",
            Self::BranchNotFound => "branch_not_found",
            Self::BranchInvalidName => "branch_invalid_name",
            Self::BranchNotFullyMerged => "branch_not_fully_merged",
            Self::BranchCheckedOut => "branch_checked_out",
            Self::BranchNoCommits => "branch_no_commits",
            Self::BranchInvalidStartPoint => "branch_invalid_start_point",
            Self::BranchUpstreamNotTracking => "branch_upstream_not_tracking",
            Self::BranchNoUpstream => "branch_no_upstream",
            Self::BranchError => "branch_error",
            Self::RevisionNotFound => "revision_not_found",
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
            Self::InvalidPattern,
            Self::Usage,
            Self::IncompatibleFlags,
            Self::IndexLocked,
            Self::BranchAlreadyExists,
            Self::BranchNotFound,
            Self::BranchInvalidName,
            Self::BranchNotFullyMerged,
            Self::BranchCheckedOut,
            Self::BranchNoCommits,
            Self::BranchInvalidStartPoint,
            Self::BranchUpstreamNotTracking,
            Self::BranchNoUpstream,
            Self::BranchError,
            Self::RevisionNotFound,
        ]
    }
}
