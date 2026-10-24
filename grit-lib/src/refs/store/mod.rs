//! Pluggable reference storage: typed transactions and backends.
//!
//! The [`RefStore`] trait models Git's ref backend vtable (prepare/commit with
//! locking, raw reads, iteration, and reflog helpers). [`MemoryRefStore`] is an
//! embeddable in-memory implementation used by tests and callers that keep refs
//! outside a repository directory.
//!
//! To write your own backend, build `prepare` and `commit` on the [`backend`] helpers and
//! check it with the `grit_test_support::refstore_conformance` suite.

mod apply;
pub mod backend;
mod error;
mod files;
mod memory;
mod open;
pub mod paths;
mod reftable;
mod routing;
mod semantics;
mod transaction;
mod types;
mod validation;

pub use error::{BackendError, RefStoreError};
pub use files::{FilesRefStore, FilesRefStoreConfig};
pub use memory::MemoryRefStore;
pub use open::open_ref_store;
pub(crate) use open::{open_files_ref_store, open_ref_store_uncached};
pub use reftable::ReftableRefStore;
pub use transaction::RefTransaction;
pub use types::{
    Expected, RawRef, RefCaseOnlyRename, RefEntry, RefName, RefReflogRename, RefStorageFormat,
    RefUpdate, RefUpdateFlags, ReflogUpdate,
};

use std::fmt::Debug;
use std::ops::ControlFlow;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// Committable ref update batch returned from [`RefStore::prepare`].
///
/// The value owns the locks `prepare` took. Exactly one of three things releases them:
/// [`Self::commit`], [`Self::abort`], or dropping the value. Implementations must release
/// their locks in `Drop` when neither method ran (callers bail out with `?` between prepare
/// and commit), leaving stored refs unchanged, so the next `prepare` does not fail with
/// [`RefStoreError::LockHeld`].
pub trait PreparedRefTransaction: Send {
    /// Apply queued updates and release locks.
    ///
    /// Every update becomes visible to readers, or (on error) none does where the backend
    /// can guarantee it.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError`] when the batch can no longer be applied.
    fn commit(self: Box<Self>) -> StoreResult<()>;

    /// Release locks without changing stored refs.
    ///
    /// # Errors
    ///
    /// Propagates backend errors while releasing resources.
    fn abort(self: Box<Self>) -> StoreResult<()>;
}

/// Reference storage backend (loose/packed files, reftable, or in-memory).
///
/// # Implementing a backend
///
/// Refs are full names (`HEAD`, `refs/heads/main`, …) mapped to a [`RawRef`]. Reads never
/// resolve symbolic refs except in [`Self::resolve`] and [`RefEntry::peeled`]. Writes only go
/// through [`Self::prepare`]; see its documentation for the checks it must make. The
/// [`backend`] module implements those checks and the commit semantics, and the
/// `grit_test_support::refstore_conformance` suite verifies a store agrees with the built-in
/// backends. Report failures of your own storage as [`RefStoreError::Backend`] (or
/// [`crate::error::Error::RefStore`] wrapping it from the `Result` methods).
pub trait RefStore: Send + Sync + Debug {
    /// Which on-disk or logical format this store uses.
    ///
    /// Custom backends return [`RefStorageFormat::Custom`].
    fn format(&self) -> RefStorageFormat;

    /// Read the stored value for `name` without resolving symbolic targets.
    ///
    /// A missing ref returns `Ok(None)` without allocating a new name string.
    fn read_raw(&self, name: &str) -> Result<Option<RawRef>>;

    /// Resolve `name` to an object id, following symbolic refs.
    ///
    /// Default implementation uses [`Self::read_raw`] and stops after
    /// [`SYMREF_MAXDEPTH`] hops.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::SymrefLoop`] when the chain is too deep and
    /// [`RefStoreError::NotFound`] when `name` or a symbolic target is missing.
    fn resolve(&self, name: &str) -> Result<ObjectId> {
        resolve_store(self, name)
    }

    /// Invoke `f` for each ref whose name starts with `prefix`, in sorted name order.
    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()>;

    /// Validate `txn`, take locks, and return a committable batch.
    ///
    /// Every check runs here, before anything is written, and a failure leaves the store
    /// unchanged and unlocked. A store must:
    ///
    /// 1. While another prepared transaction holds the lock, fail with
    ///    [`RefStoreError::LockHeld`] (do not wait indefinitely).
    /// 2. Reject names Git refuses to store with [`RefStoreError::InvalidRefName`].
    /// 3. Reject a created ref whose name is a directory prefix of a stored ref or of
    ///    another ref in the batch, or the other way round (`a` vs `a/b`), with
    ///    [`RefStoreError::NameUnavailable`]. Deletes in the same batch do not free a name.
    /// 4. Check each [`RefUpdate::expected`] against the state left by the earlier updates
    ///    in the batch ([`RefStoreError::ExpectedMismatch`]). For a direct-value update
    ///    through a symbolic ref, an [`Expected::Oid`] is compared with the resolved id.
    /// 5. Reject two updates that write the same stored ref, including through symbolic
    ///    refs, with [`RefStoreError::DuplicateUpdate`].
    ///
    /// [`backend::check_transaction`] performs steps 2–5 and [`backend::apply_transaction`]
    /// defines what [`PreparedRefTransaction::commit`] stores, including reflog lines.
    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>>;

    /// Whether a reflog is present for `name`.
    fn reflog_exists(&self, name: &str) -> Result<bool>;

    /// Iterate reflog entries for `name` (oldest first unless `reverse`).
    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()>;

    /// Create an empty reflog file/record for `name`.
    fn create_reflog(&self, name: &str) -> Result<()>;

    /// Remove the reflog for `name`.
    fn delete_reflog(&self, name: &str) -> Result<()>;

    /// Replace the entire reflog (used by expire/delete-entries).
    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()>;

    /// Invoke `f` for each ref name that has a reflog, in sorted order.
    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()>;

    /// The files backend behind this store, when it is one.
    ///
    /// Lets callers reuse state the files backend keeps between calls (such as its
    /// `packed-refs` snapshot) instead of reading the same files again. Other backends,
    /// including injected ones, return `None`.
    fn as_files(&self) -> Option<&FilesRefStore> {
        None
    }
}

/// Resolve `name` through `store`, following symbolic refs.
pub fn resolve_store<S: RefStore + ?Sized>(store: &S, name: &str) -> Result<ObjectId> {
    resolve_via_raw(store, name, 0).map_err(Into::into)
}

fn resolve_via_raw<S: RefStore + ?Sized>(
    store: &S,
    name: &str,
    depth: usize,
) -> StoreResult<ObjectId> {
    if depth >= SYMREF_MAXDEPTH {
        return Err(RefStoreError::SymrefLoop);
    }
    match store.read_raw(name).map_err(|err| match err {
        crate::error::Error::RefStore(err) => err,
        other => RefStoreError::Corrupt(other.to_string()),
    })? {
        None => Err(RefStoreError::not_found(name)),
        Some(RawRef::Direct(oid)) => Ok(oid),
        Some(RawRef::Symbolic(target)) => resolve_via_raw(store, &target, depth + 1),
    }
}
