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
    #[error("cannot lock ref: {}", reason.lock_message_suffix())]
    NameUnavailable {
        /// Structured conflict reason (Git `refs_verify_refname_available` style).
        reason: RefnameUnavailable,
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

    /// Ref name is not safe to store (traversal, illegal component, etc.).
    #[error("refusing to update ref with bad name '{name}'")]
    InvalidRefName {
        /// The rejected ref name.
        name: String,
    },
}

impl From<RefnameUnavailable> for RefStoreError {
    fn from(reason: RefnameUnavailable) -> Self {
        Self::NameUnavailable { reason }
    }
}
