//! Typed failures from diffs, patch apply, and blame.
//!
//! ```rust
//! use grit_lib::diff::error::DiffError;
//! use grit_lib::error::ApplyError;
//!
//! let err = DiffError::Apply(ApplyError::Cancelled);
//! assert_eq!(err.kind_name(), "apply");
//! ```

use crate::diff::hunk_id::HunkIdError;
use crate::diff::pickaxe::PickaxeError;
use crate::error::{ApplyError, Error};
use thiserror::Error;

/// Diff, apply, and blame failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DiffError {
    /// Unified diff / patch application failure.
    #[error(transparent)]
    Apply(#[from] ApplyError),

    /// [`crate::diff::HunkId`] parse failure.
    #[error(transparent)]
    HunkId(#[from] HunkIdError),

    /// Pickaxe pattern compilation failure.
    #[error(transparent)]
    Pickaxe(#[from] PickaxeError),

    /// `grit blame --ignore-rev` did not resolve to a commit.
    #[error("cannot find revision {rev} to ignore")]
    BlameIgnoreRevUnknown {
        /// Revision expression from `--ignore-rev`.
        rev: String,
    },

    /// `--ignore-revs-file` could not be read.
    #[error("could not open object name list: {path}")]
    BlameIgnoreRevsFileOpen {
        /// Path argument (before expansion).
        path: String,
        /// Underlying open error.
        #[source]
        source: std::io::Error,
    },

    /// A line in an ignore-revs file is not a full-length object id.
    #[error("invalid object name: {rev}")]
    BlameIgnoreRevsFileInvalidRev {
        /// Path argument for the file.
        path: String,
        /// Offending line text after comment/trim handling.
        rev: String,
    },

    /// [`crate::blame::BlameFinalSource::Worktree`] was requested in a bare repository.
    #[error("cannot blame the working tree of a bare repository")]
    BlameWorktreeBareRepository,

    /// Unknown `--diff-merges` mode name.
    #[error("unknown --diff-merges mode '{value}' (expected first-parent or combined)")]
    DiffMergesParse {
        /// Raw mode string from the user or config.
        value: String,
    },

    /// Invalid `diff.<driver>.funcname` / `xfuncname` pattern.
    #[error("invalid funcname pattern for diff driver '{driver}': {detail}")]
    UserdiffInvalidFuncname {
        /// Driver name (`diff.<driver>.*` config section).
        driver: String,
        /// Parse failure detail for the pattern text.
        detail: String,
    },

    /// Blame encountered an internal invariant violation.
    #[error("internal blame error: {detail}")]
    BlameInternal {
        /// Diagnostic detail.
        detail: String,
    },

    /// Shared worktree rules mutex was poisoned.
    #[error("worktree rules lock poisoned")]
    WorktreeRulesLockPoisoned,

    /// `HEAD` does not point at a commit for blame.
    #[error("cannot blame: HEAD does not point to a commit")]
    BlameHeadNotCommit,

    /// Too many `-C` copy-detection flags were requested.
    #[error("blame copy detection: at most three -C flags are supported")]
    BlameCopyDetectionTooManyFlags,

    /// Textconv or temporary file setup failed during blame.
    #[error("blame textconv failed: {detail}")]
    BlameTextconvFailed {
        /// Failure detail.
        detail: String,
    },

    /// Blame expected a different object kind from the ODB.
    #[error("blame expected a {expected} object")]
    BlameUnexpectedObjectKind {
        /// Expected kind name (`blob`, `tree`, …).
        expected: &'static str,
    },

    /// Underlying repository failure during diff/blame work.
    #[error(transparent)]
    Nested(#[from] Box<Error>),
}

impl DiffError {
    /// Stable snake_case kind slug matching legacy [`Error::kind_name`] values.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Apply(e) => e.kind_name(),
            Self::HunkId(_) => "hunk_id",
            Self::Pickaxe(_) => "pickaxe",
            Self::BlameIgnoreRevUnknown { .. } => "blame_ignore_rev_unknown",
            Self::BlameIgnoreRevsFileOpen { .. } => "blame_ignore_revs_file_open",
            Self::BlameIgnoreRevsFileInvalidRev { .. } => "blame_ignore_revs_file_invalid_rev",
            Self::BlameWorktreeBareRepository => "blame_worktree_bare_repository",
            Self::DiffMergesParse { .. } => "diff_merges_parse",
            Self::UserdiffInvalidFuncname { .. } => "userdiff_invalid_funcname",
            Self::BlameInternal { .. } => "blame_internal",
            Self::WorktreeRulesLockPoisoned => "worktree_rules_lock_poisoned",
            Self::BlameHeadNotCommit => "blame_head_not_commit",
            Self::BlameCopyDetectionTooManyFlags => "blame_copy_detection_too_many_flags",
            Self::BlameTextconvFailed { .. } => "blame_textconv_failed",
            Self::BlameUnexpectedObjectKind { .. } => "blame_unexpected_object_kind",
            Self::Nested(err) => err.kind_name(),
        }
    }
}

impl From<Error> for DiffError {
    fn from(err: Error) -> Self {
        match err {
            Error::Diff(de) => de,
            other => Self::Nested(Box::new(other)),
        }
    }
}
