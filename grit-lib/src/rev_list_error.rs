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
}
