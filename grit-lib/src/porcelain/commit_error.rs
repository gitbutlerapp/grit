//! Typed failures for commit, identity, signing, and patch-scoped commits.

use thiserror::Error;

use crate::config::ident_resolve::IdentityError;
use crate::error::{ApplyError, Error};
use crate::ident::CommitIdentityError;
use crate::porcelain::hunk_select::HunkSelectError;

/// Commit, identity, signing, and patch-scoped commit failures.
///
/// Wrapped by [`Error::Commit`](crate::error::Error::Commit). Stable [`kind_name`](Self::kind_name)
/// slugs match the former umbrella variant names for scripts and the CLI error map.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum CommitError {
    /// A commit was requested with an empty message body after normalization.
    #[error("empty commit message")]
    EmptyCommitMessage,

    /// [`crate::porcelain::commit::commit_with_scope`] was called with an empty
    /// [`CommitScope::Paths`](crate::porcelain::commit::CommitScope::Paths) pathspec list.
    #[error("you must specify path(s)")]
    CommitPathsRequired,

    /// [`crate::porcelain::commit::CommitScope::Paths`] was used while a merge is in progress.
    #[error("cannot do a partial commit during a merge")]
    PartialCommitDuringMerge,

    /// Partial commit, amend, or patch commit while a cherry-pick or revert is in progress.
    #[error("cannot do a partial commit during a cherry-pick or revert")]
    PartialCommitDuringReplay,

    /// The index tree matches the parent commit tree and `allow_empty` is false.
    #[error("nothing to commit")]
    NothingToCommit,

    /// No staged changes for a staged-only commit, but the work tree has unstaged or untracked paths.
    #[error("nothing staged to commit")]
    NothingStagedToCommit {
        /// Tracked paths that differ between the index and the work tree.
        unstaged: usize,
        /// Untracked paths under the work tree.
        untracked: usize,
    },

    /// [`CommitRequest::amend`](crate::porcelain::commit::CommitRequest::amend) on an unborn branch.
    #[error("cannot amend: no commits on this branch yet")]
    AmendUnborn,

    /// Author or committer on [`crate::porcelain::commit::CommitRequest`] is invalid.
    #[error(transparent)]
    InvalidIdentity(#[from] CommitIdentityError),

    /// No usable author or committer identity is configured.
    #[error(transparent)]
    Identity(#[from] IdentityError),

    /// Commit or tag signing failed.
    #[error("commit signing failed")]
    Signing {
        /// Signing program invoked (`gpg`, `ssh-keygen`, …).
        program: String,
        /// Subprocess exit status when the program ran.
        status: Option<i32>,
        /// Subprocess stderr (or a short summary when spawn failed).
        stderr: String,
    },

    /// Hunk id resolution failed during a patch-scoped commit.
    #[error(transparent)]
    HunkSelect(#[from] HunkSelectError),

    /// Patch apply failed after hunk resolution.
    #[error(transparent)]
    PatchApply(#[from] ApplyError),

    /// Underlying repository, hook, index, or I/O failure.
    #[error(transparent)]
    Library(Box<Error>),
}

impl CommitError {
    /// Stable snake_case slug for this failure (matches former [`Error::kind_name`] values).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::EmptyCommitMessage => "empty_commit_message",
            Self::CommitPathsRequired => "commit_paths_required",
            Self::PartialCommitDuringMerge => "partial_commit_during_merge",
            Self::PartialCommitDuringReplay => "partial_commit_during_replay",
            Self::NothingToCommit => "nothing_to_commit",
            Self::NothingStagedToCommit { .. } => "nothing_staged_to_commit",
            Self::AmendUnborn => "amend_unborn",
            Self::InvalidIdentity(..) => "invalid_identity",
            Self::Identity(..) => "identity",
            Self::Signing { .. } => "signing",
            Self::HunkSelect(..) => "hunk_selection",
            Self::PatchApply(..) => "apply",
            Self::Library(e) => e.kind_name(),
        }
    }
}

impl CommitError {
    /// Build [`Self::Signing`] from a legacy single-string signing failure summary.
    #[must_use]
    pub fn signing_from_message(message: String) -> Self {
        Self::Signing {
            program: String::new(),
            status: None,
            stderr: message,
        }
    }
}

impl From<Error> for CommitError {
    fn from(err: Error) -> Self {
        match err {
            Error::Commit(inner) => inner,
            Error::HunkSelection(e) => Self::HunkSelect(e),
            Error::Apply(e) => Self::PatchApply(e),
            other => Self::Library(Box::new(other)),
        }
    }
}
