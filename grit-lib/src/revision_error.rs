//! Typed failures from revision parsing, walking, merge-base, and object filters.

use crate::error::Error;
use crate::objects::ObjectId;
use crate::rev_list_error::RevListError;
use crate::rev_parse_error::RevParseError;
use thiserror::Error;

/// Revision parsing, rev-list, merge-base, and object-filter failures.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RevisionError {
    /// Revision expression names nothing in the repository.
    #[error("unknown revision '{spec}'")]
    NotFound {
        /// Expression as the caller wrote it.
        spec: String,
    },

    /// Two commits share no merge base where one was required.
    #[error("commits {ours} and {theirs} have no common ancestor")]
    UnrelatedHistories {
        /// Commit being merged into.
        ours: ObjectId,
        /// Commit being merged in.
        theirs: ObjectId,
    },

    /// [`crate::rev_parse`] failure.
    #[error(transparent)]
    RevParse(#[from] RevParseError),

    /// [`crate::rev_list`] / rev-walk option failure.
    #[error(transparent)]
    RevList(#[from] RevListError),

    /// No merge base between commits (for example `git diff --merge-base`).
    #[error("no merge base")]
    NoMergeBase,

    /// More than one minimal merge base (criss-cross history).
    #[error("multiple merge bases")]
    MultipleMergeBases,

    /// `--filter=` / [`crate::rev_list::ObjectFilter::parse`] failure.
    #[error(transparent)]
    ObjectFilterParse(#[from] ObjectFilterParseError),

    /// [`crate::rev_list::expand_object_filter_for_protocol`] failure.
    #[error(transparent)]
    ObjectFilterProtocol(#[from] ObjectFilterProtocolError),

    /// Underlying repository or I/O failure during revision work.
    #[error(transparent)]
    Nested(#[from] Box<Error>),
}

/// Invalid `--filter=` specification.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectFilterParseError {
    /// Top-level filter string is not recognized.
    #[error("invalid filter-spec '{spec}'")]
    InvalidFilterSpec {
        /// Original `--filter` value.
        spec: String,
    },

    /// A fragment of a filter (for example inside `combine:`) is invalid.
    #[error("{detail}")]
    InvalidFragment {
        /// Parser detail (matches prior string errors).
        detail: String,
    },
}

/// Failure expanding a filter for protocol negotiation.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ObjectFilterProtocolError {
    /// Underlying parse failure while expanding.
    #[error(transparent)]
    Parse(#[from] ObjectFilterParseError),
}

impl RevisionError {
    /// Stable snake_case kind slug matching legacy [`Error::kind_name`] values.
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "revision_not_found",
            Self::UnrelatedHistories { .. } => "unrelated_histories",
            Self::RevParse(_) => "rev_parse",
            Self::RevList(_) => "rev_list",
            Self::NoMergeBase => "no_merge_base",
            Self::MultipleMergeBases => "multiple_merge_bases",
            Self::ObjectFilterParse(_) => "object_filter_parse",
            Self::ObjectFilterProtocol(_) => "object_filter_protocol",
            Self::Nested(err) => err.kind_name(),
        }
    }
}

impl From<ObjectFilterParseError> for Error {
    fn from(err: ObjectFilterParseError) -> Self {
        Self::Revision(RevisionError::ObjectFilterParse(err))
    }
}

impl From<ObjectFilterProtocolError> for Error {
    fn from(err: ObjectFilterProtocolError) -> Self {
        Self::Revision(RevisionError::ObjectFilterProtocol(err))
    }
}

impl From<Error> for RevisionError {
    fn from(err: Error) -> Self {
        match err {
            Error::Revision(re) => re,
            other => Self::Nested(Box::new(other)),
        }
    }
}
