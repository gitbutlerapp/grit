//! Errors for [`super::RefStore`] operations.

use crate::refs::RefnameUnavailable;

use super::{Expected, RawRef};

/// Failure modes for reference store reads and transactions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RefStoreError {
    /// Another prepared transaction holds the store lock.
    #[error("cannot lock ref '{name}': lock held")]
    LockHeld {
        /// Ref name associated with the active lock (often the first update name).
        name: String,
    },

    /// Compare-and-swap or existence check failed for an update.
    #[error("ref '{name}' did not match expected state")]
    ExpectedMismatch {
        /// Ref that failed the check.
        name: String,
        /// What the caller required.
        expected: Expected,
        /// What was observed (`None` when the ref is absent).
        actual: Option<RawRef>,
    },

    /// A ref name cannot be created because of a directory/file conflict.
    #[error("cannot lock ref: {detail}")]
    NameUnavailable {
        /// Human-readable conflict detail (Git lock message suffix).
        detail: String,
    },

    /// The same ref name appears more than once in one transaction.
    #[error("multiple updates for ref '{name}' not allowed")]
    DuplicateUpdate {
        /// Duplicate ref name.
        name: String,
    },

    /// Symbolic reference resolution exceeded [`crate::refs::SYMREF_MAXDEPTH`].
    #[error("symbolic ref loop or chain too deep")]
    SymrefLoop,

    /// Stored data for a ref or reflog is malformed.
    #[error("corrupt ref store: {0}")]
    Corrupt(String),
}

impl From<RefnameUnavailable> for RefStoreError {
    fn from(value: RefnameUnavailable) -> Self {
        Self::NameUnavailable {
            detail: value.lock_message_suffix(),
        }
    }
}
