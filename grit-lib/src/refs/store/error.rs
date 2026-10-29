//! Errors for [`super::RefStore`] operations.

use std::fmt;
use std::sync::Arc;

use crate::git_dir::GitDirPathOperation;
use crate::objects::{ObjectId, ObjectKind};
use crate::refs::RefnameUnavailable;

use super::{Expected, RawRef};

/// Failure modes for reference store reads and transactions.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum RefStoreError {
    /// Another writer holds a lock this transaction needs.
    ///
    /// For per-ref backends (files) that is the lock of `name` itself; for a backend with one
    /// writer slot for the whole store ([`super::MemoryRefStore`], a database with a single
    /// write transaction) it means that slot is busy, whichever refs the holder updates.
    /// Transient: [`crate::refs::prepare_with_lock_wait`] retries it.
    #[error("cannot lock ref '{name}': lock held")]
    LockHeld {
        /// A ref of the failed transaction: the locked ref, or the transaction's first ref
        /// when the whole store is locked. Always one of the failed transaction's own refs.
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
    #[error("cannot lock ref '{}': {}", reason.refname(), reason.lock_message_suffix())]
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

    /// Filesystem I/O on a git-directory path during ref store work.
    #[error("{}", crate::git_dir::io_error::format_git_dir_io_message(.path, *operation, &Self::git_dir_io_source(*.os_error)))]
    GitDirPathIo {
        /// Display path such as `.git/refs/heads/main` or `.git/worktrees/wt/HEAD.lock`.
        path: String,
        /// What the operation was trying to do when I/O failed.
        operation: GitDirPathOperation,
        /// Raw OS error code from the failing I/O call.
        os_error: i32,
    },

    /// Ref name is not safe to store (traversal, illegal component, etc.).
    ///
    /// Every backend checks update names and both sides of reflog and case-only renames
    /// in [`super::RefStore::prepare`], before taking locks.
    #[error("refusing to update ref with bad name '{name}'")]
    InvalidRefName {
        /// The rejected ref name.
        name: String,
    },

    /// A symbolic ref would point at a target Git refuses (outside `refs/` for `HEAD`, not
    /// safe to store, or the same name as the ref being updated).
    #[error("refusing to point '{name}' at symbolic target '{target}'")]
    InvalidSymrefTarget {
        /// The ref being updated.
        name: String,
        /// The symbolic target that was rejected.
        target: String,
    },

    /// An update would store the all-zero (null) object id as a direct ref value.
    ///
    /// The null id names no object, so Git treats such a ref as broken. Delete a ref with
    /// `new_value: None` instead. Reflog-only updates
    /// ([`super::RefUpdateFlags::log_only`]) may still carry the null id.
    #[error("refusing to point ref '{name}' at the null object id")]
    NullOid {
        /// The ref the update targeted.
        name: String,
    },

    /// An update would point a ref at an object the repository does not store.
    ///
    /// Raised by the stores a [`crate::repo::Repository`] hands out (see
    /// [`super::RefStore::prepare`]); the bare backends do not look at objects. Set
    /// [`super::RefUpdateFlags::skip_oid_verification`] to write such a ref on purpose.
    #[error("refusing to point ref '{name}' at {oid}: no such object in the repository")]
    MissingObject {
        /// The ref the update targeted.
        name: String,
        /// The object id the update would store.
        oid: ObjectId,
    },

    /// An update would point a branch (`refs/heads/…`) or `HEAD` at something other than a
    /// commit.
    ///
    /// Checked alongside [`Self::MissingObject`]. Tags, remote-tracking and other refs may
    /// name any object.
    #[error("refusing to point '{name}' at {kind} {oid}: branches and HEAD must name a commit")]
    NotACommit {
        /// The branch or `HEAD` the update targeted.
        name: String,
        /// The object id the update would store.
        oid: ObjectId,
        /// What `oid` actually is.
        kind: ObjectKind,
    },

    /// Looking up the object an update would store failed (unreadable pack, I/O, a failing
    /// custom object store), so the update could not be checked.
    #[error("cannot check object {oid} for ref '{name}': {source}")]
    ObjectLookup {
        /// The ref the update targeted.
        name: String,
        /// The object id the update would store.
        oid: ObjectId,
        /// The object database error, downcastable to [`crate::error::Error`].
        source: BackendError,
    },

    /// Reflog identity or message would forge or corrupt a log line.
    #[error(transparent)]
    InvalidReflog(#[from] crate::ident::SignatureError),

    /// A custom backend's own storage failed (database, network, …).
    ///
    /// Built-in backends never return this; their I/O failures stay typed elsewhere.
    #[error(transparent)]
    Backend(BackendError),

    /// A `reference-transaction` hook refused or could not run on the `prepared` phase.
    #[error(transparent)]
    Hook(ReferenceTransactionHookErr),
}

/// [`RefStoreError::Hook`] payload ( [`crate::hooks::HookError`] is not `Clone`).
#[derive(Clone, Debug)]
pub struct ReferenceTransactionHookErr(Arc<crate::hooks::HookError>);

impl PartialEq for ReferenceTransactionHookErr {
    fn eq(&self, other: &Self) -> bool {
        self.0.to_string() == other.0.to_string()
    }
}

impl Eq for ReferenceTransactionHookErr {}

impl From<crate::hooks::HookError> for ReferenceTransactionHookErr {
    fn from(value: crate::hooks::HookError) -> Self {
        Self(Arc::new(value))
    }
}

impl ReferenceTransactionHookErr {
    pub(crate) fn into_hook_error(self) -> crate::hooks::HookError {
        Arc::try_unwrap(self.0).unwrap_or_else(|arc| match arc.as_ref() {
            crate::hooks::HookError::Failed {
                hook_name,
                status,
                output,
            } => crate::hooks::HookError::Failed {
                hook_name: hook_name.clone(),
                status: *status,
                output: output.clone(),
            },
            crate::hooks::HookError::Spawn {
                hook_name,
                path,
                source,
                output,
            } => crate::hooks::HookError::Spawn {
                hook_name: hook_name.clone(),
                path: path.clone(),
                source: std::io::Error::new(source.kind(), source.to_string()),
                output: output.clone(),
            },
            crate::hooks::HookError::Internal(msg) => {
                crate::hooks::HookError::Internal(msg.clone())
            }
        })
    }
}

impl std::fmt::Display for ReferenceTransactionHookErr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl std::error::Error for ReferenceTransactionHookErr {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

impl RefStoreError {
    pub(crate) fn git_dir_path_io(
        path: String,
        operation: GitDirPathOperation,
        source: std::io::Error,
    ) -> Self {
        Self::GitDirPathIo {
            path,
            operation,
            os_error: source.raw_os_error().unwrap_or(0),
        }
    }

    pub(crate) fn git_dir_io_source(os_error: i32) -> std::io::Error {
        if os_error == 0 {
            std::io::Error::from(std::io::ErrorKind::Other)
        } else {
            std::io::Error::from_raw_os_error(os_error)
        }
    }

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
