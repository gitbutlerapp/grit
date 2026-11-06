//! Typed failures for stash list, push, apply, pop, and drop.

use thiserror::Error;

use crate::error::PathIoError;
use crate::objects::ObjectId;
use crate::porcelain::branch_error::BranchError;
use crate::porcelain::hunk_select::HunkSelectError;

/// Result alias for stash stack operations.
pub type StashResult<T> = std::result::Result<T, StashError>;

/// Why a stash commit object cannot be interpreted for apply, show, or branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CorruptStashReason {
    /// The W commit has fewer than two parents (index and base).
    InsufficientParents,
}

/// Incompatible [`super::stash::StashPushOptions`](crate::stash::StashPushOptions) flag combinations.
///
/// Returned from [`crate::stash::StashPushOptions::validate`] before any repository I/O.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StashPushValidationError {
    /// Author or committer line on [`crate::stash::StashPushOptions::identity`] is invalid.
    #[error(transparent)]
    InvalidSignature(#[from] crate::ident::SignatureError),

    /// `--staged` with `--include-untracked`.
    #[error("cannot combine --staged with --include-untracked")]
    StagedWithUntracked,
    /// `--staged` with `--hunks` or `--lines`.
    #[error("cannot combine --staged with --hunks or --lines")]
    StagedWithSelection,
    /// `--keep-index` with `--hunks` or `--lines`.
    #[error("cannot combine --keep-index with --hunks or --lines")]
    KeepIndexWithSelection,
    /// `--include-untracked` with `--hunks` or `--lines`.
    #[error("cannot combine --include-untracked with --hunks or --lines")]
    UntrackedWithSelection,
    /// `--staged` on a path that also has unstaged worktree edits.
    #[error("cannot stash staged changes for '{path}': worktree has unstaged changes")]
    StagedWorktreeDirty {
        /// Repository-relative path.
        path: String,
    },
    /// [`crate::stash::StashPushOptions::dry_run`] on push; use preview instead.
    #[error("dry-run is not supported on push; use preview instead")]
    DryRunUsePreview,
}

/// Stash list, push, apply, pop, drop, and branch failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum StashError {
    /// Underlying object, ref, index, checkout, or revision failure.
    #[error(transparent)]
    Library(#[from] Box<crate::error::Error>),

    /// Stash W commit has an invalid parent layout.
    #[error("corrupt stash commit {oid}: {reason}")]
    CorruptStash {
        /// Stash W commit object id.
        oid: ObjectId,
        /// What was wrong with the commit graph.
        reason: CorruptStashReason,
    },

    /// `stash@{{n}}` is out of range for the current stash reflog.
    #[error("stash entry stash@{{{n}}} not found")]
    NotFound {
        /// Stash reflog index (`stash@{{n}}`).
        n: usize,
    },

    /// A stash commit id was given to pop but is not on `refs/stash`.
    #[error("stash commit {oid} is not on the stash stack")]
    CommitNotOnStack {
        /// Stash W commit object id.
        oid: ObjectId,
    },

    /// Push or create requires at least one commit on the current branch.
    #[error("cannot stash without a commit")]
    NoInitialCommit,

    /// Apply, push, or pop requires a working tree.
    #[error("cannot stash in a bare repository")]
    BareRepository,

    /// Apply with `restore_index` could not merge the current index with the stashed index parent.
    #[error("conflicts in index while applying stash with --index")]
    IndexConflict,

    /// `HEAD` does not resolve while applying a stash entry.
    #[error("missing HEAD while applying stash")]
    MissingHead,

    /// Applying a stash would overwrite locally modified worktree content.
    #[error(
        "your local changes to the following files would be overwritten by stash apply: {paths}"
    )]
    WouldOverwriteLocalChanges {
        /// Space-separated paths (same layout as Git's message list).
        paths: String,
    },

    /// The index could not be written after a successful stash apply.
    #[error("writing index after stash apply")]
    IndexWrite {
        /// Git-directory path I/O failure.
        #[source]
        source: PathIoError,
    },

    /// A symlink blob in a stash commit is not valid UTF-8.
    #[error("symlink target is not UTF-8")]
    SymlinkNotUtf8,

    /// Applying a stash with an untracked-files parent would replace an existing worktree path.
    #[error("stash untracked restore blocked: '{path}' already exists on disk")]
    CouldNotRestoreUntracked {
        /// Worktree path that already exists and blocks untracked restore.
        path: String,
    },

    /// Partial push hunk selection failed.
    #[error(transparent)]
    HunkSelection(#[from] HunkSelectError),

    /// Branch create during `stash branch` failed validation or state checks.
    #[error(transparent)]
    Branch(#[from] BranchError),

    /// Patch application while building the W tree failed.
    #[error(transparent)]
    Apply(#[from] crate::error::ApplyError),

    /// Identity on push options could not be written into a commit.
    #[error(transparent)]
    InvalidSignature(#[from] crate::ident::SignatureError),

    /// Operation stopped via [`CancelToken`](crate::progress::CancelToken).
    #[error("operation cancelled")]
    Cancelled,

    /// Incompatible [`crate::stash::StashPushOptions`] flags (see [`StashPushValidationError`]).
    #[error(transparent)]
    PushValidation(#[from] StashPushValidationError),

    /// Worktree path update failed while finishing a stash push reset.
    #[error("could not update '{path}' while finishing stash: {message}")]
    WorktreeUpdateFailed {
        /// Repository-relative path.
        path: String,
        /// Apply or checkout detail.
        message: String,
    },
}

impl StashError {
    pub(crate) fn corrupt_stash(oid: ObjectId, reason: CorruptStashReason) -> Self {
        Self::CorruptStash { oid, reason }
    }

    pub(crate) fn library(err: crate::error::Error) -> Self {
        match err {
            crate::error::Error::Cancelled => Self::Cancelled,
            other => Self::Library(Box::new(other)),
        }
    }

    pub(crate) fn from_apply_worktree(err: crate::error::ApplyError) -> Self {
        match err {
            crate::error::ApplyError::Cancelled => Self::Cancelled,
            crate::error::ApplyError::Failed { path, message } if !path.is_empty() => {
                Self::WorktreeUpdateFailed { path, message }
            }
            other => Self::Apply(other),
        }
    }

    /// Map into the library umbrella for non-stash APIs (merge autostash, pull, and similar).
    pub(crate) fn into_error(self) -> crate::error::Error {
        match self {
            Self::Cancelled => crate::error::Error::Cancelled,
            Self::Library(boxed) => *boxed,
            other => crate::error::Error::Message(other.to_string()),
        }
    }
}

impl From<crate::error::Error> for StashError {
    fn from(err: crate::error::Error) -> Self {
        match err {
            crate::error::Error::Cancelled => Self::Cancelled,
            other => Self::Library(Box::new(other)),
        }
    }
}

impl From<crate::index::IndexError> for StashError {
    fn from(err: crate::index::IndexError) -> Self {
        Self::library(crate::error::Error::from(err))
    }
}

impl From<StashError> for crate::error::Error {
    fn from(err: StashError) -> Self {
        err.into_error()
    }
}

#[cfg(feature = "serde-models")]
impl StashError {
    /// Stable `kind` slug when this error is serialized in the legacy `Error` JSON shape.
    pub(crate) fn legacy_kind(&self) -> &'static str {
        match self {
            Self::CorruptStash { .. } => "corrupt_stash",
            Self::NotFound { .. } => "stash_not_found",
            Self::CommitNotOnStack { .. } => "stash_commit_not_on_stack",
            Self::NoInitialCommit => "stash_no_initial_commit",
            Self::BareRepository => "stash_bare_repository",
            Self::IndexConflict => "stash_index_conflict",
            Self::MissingHead => "stash_missing_head",
            Self::WouldOverwriteLocalChanges { .. } => "stash_would_overwrite_local_changes",
            Self::IndexWrite { .. } => "stash_index_write",
            Self::SymlinkNotUtf8 => "stash_symlink_not_utf8",
            Self::CouldNotRestoreUntracked { .. } => "stash_could_not_restore_untracked",
            Self::PushValidation(_) => "stash_option_conflict",
            Self::Cancelled => "cancelled",
            Self::HunkSelection(_) => "hunk_selection",
            Self::Branch(_) => "branch",
            Self::Apply(_) => "apply",
            Self::InvalidSignature(_) => "invalid_signature",
            Self::WorktreeUpdateFailed { .. } => "stash_worktree_update_failed",
            Self::Library(boxed) => boxed.kind_name(),
        }
    }
}

#[cfg(feature = "serde-models")]
impl serde::Serialize for StashError {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;

        match self {
            Self::CouldNotRestoreUntracked { path } => {
                let mut out = serializer.serialize_struct("Error", 3)?;
                out.serialize_field("kind", self.legacy_kind())?;
                out.serialize_field("message", &self.to_string())?;
                out.serialize_field("path", path)?;
                out.end()
            }
            _ => {
                let mut out = serializer.serialize_struct("Error", 2)?;
                out.serialize_field("kind", self.legacy_kind())?;
                out.serialize_field("message", &self.to_string())?;
                out.end()
            }
        }
    }
}

impl std::fmt::Display for CorruptStashReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientParents => f.write_str("expected at least 2 parents"),
        }
    }
}
