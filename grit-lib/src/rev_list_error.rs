//! Typed failures from [`crate::rev_list`].

use thiserror::Error;

/// Failure modes when walking revisions or parsing `rev-list` options.
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum RevListError {
    /// Sparse filter blob bytes are not valid UTF-8 or not a blob object.
    #[error("unable to parse sparse filter data in {object_id}")]
    SparseFilterUnparsable {
        /// Hex object id of the blob that was read.
        object_id: String,
    },

    /// `sparse:oid=` spec did not resolve to a readable blob.
    #[error("unable to access sparse blob in '{spec}'")]
    SparseBlobUnreadable {
        /// Original filter spec fragment.
        spec: String,
    },

    /// A long option in `--stdin` mode requires a value on the following line.
    #[error("option '{option}' requires a value")]
    MissingOptionValue {
        /// Option name without leading `--`.
        option: String,
    },

    /// Unrecognized or malformed pseudo-option while reading `--stdin` revisions.
    #[error("invalid option '{line}' in --stdin mode")]
    InvalidStdinOption {
        /// Raw stdin line.
        line: String,
    },

    /// `--no-walk=` was given a value other than `sorted` or `unsorted`.
    #[error("invalid argument to --no-walk")]
    InvalidNoWalkArgument,

    /// [`crate::revwalk::RevWalk::left_right_counts`] requires a single `A...B` token.
    #[error("symmetric revision range (A...B) required for left/right commit counts")]
    SymmetricRangeRequired,

    /// [`crate::revwalk::RevWalk::walk`] got something other than one revision or range: an
    /// option such as `--all`, the `--` separator, or several whitespace-separated tokens.
    ///
    /// Use [`crate::revwalk::RevWalk::commits`] with
    /// [`crate::revwalk::RevWalkListOptions::refs`] / [`crate::revwalk::RevWalkListOptions::paths`]
    /// for ref selections, path limits, and multiple tokens.
    #[error("'{spec}' is not a single revision or range; list several tokens, refs or paths with RevWalk::commits")]
    UnsupportedWalkSpec {
        /// The spec exactly as passed to `walk`.
        spec: String,
    },

    /// [`crate::rev_list::rev_list`] got a raw `A...B` spec; symmetric ranges must be expanded
    /// first with [`crate::revwalk::parse_rev_walk_tokens`] (positive tips, merge-base
    /// exclusions, and the `symmetric_left` / `symmetric_right` options).
    #[error(
        "symmetric range '{spec}' must be expanded with parse_rev_walk_tokens before rev_list"
    )]
    UnexpandedSymmetricRange {
        /// The unexpanded `A...B` spec.
        spec: String,
    },

    /// Object walk expected a tree, blob, commit, or tag but read another kind from the ODB (upstream `t6102`).
    #[error("object {input} is not a {expected}")]
    UnexpectedObjectKind {
        /// User-facing revision or path spec.
        input: String,
        /// Expected object type name (`tree`, `blob`, `commit`, `tag`).
        expected: &'static str,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;

    #[test]
    fn variant_sparse_filter_unparsable() {
        let err = Error::from(RevListError::SparseFilterUnparsable {
            object_id: "abc".into(),
        });
        assert!(matches!(
            err,
            Error::RevList(RevListError::SparseFilterUnparsable { .. })
        ));
    }

    #[test]
    fn variant_missing_option_value() {
        let err = Error::from(RevListError::MissingOptionValue {
            option: "glob".into(),
        });
        assert!(matches!(
            err,
            Error::RevList(RevListError::MissingOptionValue { .. })
        ));
    }

    #[test]
    fn display_is_neutral() {
        let msg = RevListError::InvalidNoWalkArgument.to_string();
        assert!(!msg.contains("fatal:"));
        assert!(!msg.contains("error:"));
    }

    #[test]
    fn walk_spec_variants_keep_the_spec() {
        let unsupported = RevListError::UnsupportedWalkSpec {
            spec: "--all".into(),
        };
        assert!(unsupported.to_string().contains("'--all'"));
        let symmetric = RevListError::UnexpandedSymmetricRange {
            spec: "a...b".into(),
        };
        assert!(symmetric.to_string().contains("'a...b'"));
        assert!(matches!(
            Error::from(symmetric),
            Error::RevList(RevListError::UnexpandedSymmetricRange { .. })
        ));
    }

    #[test]
    fn unexpected_object_kind_display() {
        let err = RevListError::UnexpectedObjectKind {
            input: "abc".into(),
            expected: "tree",
        };
        assert_eq!(err.to_string(), "object abc is not a tree");
    }
}
