//! [`RevisionError`]: revision lookup, parsing, and walking failures.

use thiserror::Error;

/// Revision failure: a name that resolves to nothing, a revision expression that does not
/// parse, or a revision walk whose options are invalid.
///
/// Surfaced on the umbrella as [`Error::Revision`](crate::error::Error::Revision).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RevisionError {
    /// A revision expression (branch, tag, object id, `x~n`, `ORIG_HEAD`, …) names nothing in
    /// the repository.
    #[error("unknown revision '{spec}'")]
    NotFound {
        /// The expression as the caller wrote it.
        spec: String,
    },

    /// Two commits share no common ancestor.
    #[error("no merge base between {left} and {right}")]
    NoMergeBase {
        /// First commit.
        left: crate::objects::ObjectId,
        /// Second commit.
        right: crate::objects::ObjectId,
    },

    /// Two commits have more than one best common ancestor (criss-cross history), and the
    /// operation needs exactly one.
    #[error("multiple merge bases between {left} and {right}")]
    MultipleMergeBases {
        /// First commit.
        left: crate::objects::ObjectId,
        /// Second commit.
        right: crate::objects::ObjectId,
    },

    /// A revision spec could not be resolved to an object for inspection.
    #[error(transparent)]
    Resolve(#[from] crate::object_inspect::ResolveError),

    /// Revision parsing failed.
    #[error(transparent)]
    Parse(#[from] crate::rev_parse_error::RevParseError),

    /// Revision walking or `rev-list` option parsing failed.
    #[error(transparent)]
    List(#[from] crate::rev_list_error::RevListError),
}

impl RevisionError {
    /// Stable snake_case slug for this failure (the former umbrella variant names).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => "revision_not_found",
            Self::NoMergeBase { .. } => "no_merge_base",
            Self::MultipleMergeBases { .. } => "multiple_merge_bases",
            Self::Parse(..) => "rev_parse",
            Self::Resolve(..) => "resolve",
            Self::List(..) => "rev_list",
        }
    }
}
