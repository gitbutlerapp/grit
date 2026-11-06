//! Typed failures for reference storage, resolution, and reflog selection.

use std::num::ParseIntError;

use thiserror::Error;

use crate::error::RefLockError;
use crate::refs::store::RefStoreError;
use crate::reftable::ReftableError;

/// Reference storage, resolution, HEAD state, and reflog failures.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum RefError {
    /// A reference name or stored value is invalid.
    #[error("invalid ref: {reason}")]
    InvalidName {
        /// Ref name when known (empty when only a free-form reason is available).
        name: String,
        /// What is wrong with the name or value.
        reason: String,
    },

    /// A `packed-refs` line Git would reject when reading the ref database.
    #[error("unexpected line in {path}: {line}")]
    PackedRefsUnexpectedLine {
        /// Path to the `packed-refs` file being read.
        path: String,
        /// Offending line contents.
        line: String,
    },

    /// Reference lock failure while creating or updating a ref.
    #[error(transparent)]
    RefLock(#[from] RefLockError),

    /// Reference store transaction or iteration failure.
    #[error(transparent)]
    Store(#[from] RefStoreError),

    /// Reftable file, stack, or transaction failure.
    #[error(transparent)]
    Reftable(#[from] ReftableError),

    /// [`create_commit`](crate::porcelain::commit::create_commit) requires a branch checkout.
    #[error("HEAD is detached")]
    DetachedHead,

    /// `HEAD` is on a branch with no commits yet, but the operation needs a commit.
    #[error("no commits yet on this branch")]
    UnbornHead,

    /// `HEAD` could not be resolved to a branch, detached commit, or unborn branch.
    #[error("HEAD is in an unknown state")]
    InvalidHead,

    /// Ref does not exist and has no reflog entries.
    #[error("unknown ref '{refname}'")]
    ReflogUnknownRef {
        /// User-supplied or DWIM ref name.
        refname: String,
    },

    /// A reflog selector is missing its reference or `@{n}` delimiters.
    #[error("expected ref@{{n}} form, got '{spec}'")]
    ReflogInvalidSelector {
        /// Selector after trimming surrounding whitespace.
        spec: String,
    },

    /// A reflog selector index is not a nonnegative integer that fits in `usize`.
    #[error("invalid reflog index in '{spec}'")]
    ReflogInvalidIndex {
        /// Selector after trimming surrounding whitespace.
        spec: String,
        /// Integer parsing failure, including overflow.
        #[source]
        source: ParseIntError,
    },

    /// `ref@{{n}}` is out of range for the current reflog.
    #[error("reflog entry {refname}@{{{index}}} not found")]
    ReflogIndexOutOfRange {
        /// Ref whose reflog was read.
        refname: String,
        /// Requested newest-first index.
        index: usize,
    },
}

impl RefError {
    /// Stable snake_case name for scripts (matches legacy [`crate::error::Error::kind_name`] slugs).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::InvalidName { .. } => "invalid_ref",
            Self::PackedRefsUnexpectedLine { .. } => "packed_refs_unexpected_line",
            Self::RefLock(_) => "ref_lock",
            Self::Store(_) => "ref_store",
            Self::Reftable(_) => "reftable",
            Self::DetachedHead => "detached_head",
            Self::UnbornHead => "unborn_head",
            Self::InvalidHead => "invalid_head",
            Self::ReflogUnknownRef { .. } => "reflog_unknown_ref",
            Self::ReflogInvalidSelector { .. } => "reflog_invalid_selector",
            Self::ReflogInvalidIndex { .. } => "reflog_invalid_index",
            Self::ReflogIndexOutOfRange { .. } => "reflog_index_out_of_range",
        }
    }

    /// Build [`Self::InvalidName`] when only a single message is available.
    #[must_use]
    pub fn invalid_reason(reason: impl Into<String>) -> Self {
        Self::InvalidName {
            name: String::new(),
            reason: reason.into(),
        }
    }

    /// Build [`Self::InvalidName`] with an explicit ref name.
    #[must_use]
    pub fn invalid_name(name: impl Into<String>, reason: impl Into<String>) -> Self {
        Self::InvalidName {
            name: name.into(),
            reason: reason.into(),
        }
    }
}

/// Result alias for ref operations that return [`RefError`].
pub type RefResult<T> = std::result::Result<T, RefError>;
