//! Errors for [`super::RefStore`] operations.

use std::fmt;
use std::sync::Arc;

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

    /// The ref (or the target at the end of its symbolic chain) does not exist.
    #[error("ref not found: {name}")]
    NotFound {
        /// Name of the missing ref: the requested name, or the dangling symbolic target.
        name: String,
    },

    /// Stored data for a ref or reflog is malformed.
    #[error("corrupt ref store: {0}")]
    Corrupt(String),

    /// Ref name is not safe to store (traversal, illegal component, etc.).
    #[error("refusing to update ref with bad name '{name}'")]
    InvalidRefName {
        /// The rejected ref name.
        name: String,
    },

    /// A custom backend's own storage failed (database, network, …).
    ///
    /// Built-in backends never return this; their I/O failures stay typed elsewhere.
    #[error(transparent)]
    Backend(BackendError),
}

impl RefStoreError {
    pub(crate) fn not_found(name: &str) -> Self {
        Self::NotFound {
            name: name.to_owned(),
        }
    }

    /// Wrap a custom backend's storage error as [`Self::Backend`].
    pub fn backend(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self::Backend(BackendError::new(err))
    }
}

/// A custom ref backend's storage error, carried by [`RefStoreError::Backend`].
///
/// Cheap to clone (the error is shared). Two values are equal only when they share the same
/// underlying error. Use [`Self::downcast_ref`] to recover the backend's concrete type.
#[derive(Clone)]
pub struct BackendError(Arc<dyn std::error::Error + Send + Sync + 'static>);

impl BackendError {
    /// Wrap `err`.
    pub fn new(err: impl std::error::Error + Send + Sync + 'static) -> Self {
        Self(Arc::new(err))
    }

    /// Borrow the wrapped error.
    #[must_use]
    pub fn get_ref(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
        self.0.as_ref()
    }

    /// The wrapped error as `E`, when it has that type.
    #[must_use]
    pub fn downcast_ref<E: std::error::Error + 'static>(&self) -> Option<&E> {
        self.0.downcast_ref::<E>()
    }
}

impl fmt::Debug for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.0, f)
    }
}

impl fmt::Display for BackendError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

impl std::error::Error for BackendError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.source()
    }
}

impl PartialEq for BackendError {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for BackendError {}

impl From<BackendError> for RefStoreError {
    fn from(err: BackendError) -> Self {
        Self::Backend(err)
    }
}

impl From<RefnameUnavailable> for RefStoreError {
    fn from(reason: RefnameUnavailable) -> Self {
        Self::NameUnavailable { reason }
    }
}
