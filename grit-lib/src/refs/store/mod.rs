//! Pluggable reference storage: typed transactions and backends.
//!
//! The [`RefStore`] trait models Git's ref backend vtable (prepare/commit with
//! locking, raw reads, iteration, and reflog helpers). [`MemoryRefStore`] is an
//! embeddable in-memory implementation used by tests and callers that keep refs
//! outside a repository directory.

mod error;
mod memory;
mod transaction;
mod types;
mod validation;

pub use error::RefStoreError;
pub use memory::MemoryRefStore;
pub use transaction::RefTransaction;
pub use types::{
    Expected, RawRef, RefEntry, RefStorageFormat, RefUpdate, RefUpdateFlags, ReflogUpdate,
};

use std::fmt::Debug;
use std::ops::ControlFlow;

use crate::error::Result;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// Committable ref update batch returned from [`RefStore::prepare`].
pub trait PreparedRefTransaction: Send {
    /// Apply queued updates and release locks.
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
pub trait RefStore: Send + Sync + Debug {
    /// Which on-disk or logical format this store uses.
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
    /// [`RefStoreError::Corrupt`] when an intermediate target is missing.
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
    /// All compare-and-swap and directory/file availability checks run before
    /// returning; calling [`PreparedRefTransaction::commit`] applies the batch atomically.
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
    match store
        .read_raw(name)
        .map_err(|err| RefStoreError::Corrupt(err.to_string()))?
    {
        None => Err(RefStoreError::Corrupt(format!("ref not found: {name}"))),
        Some(RawRef::Direct(oid)) => Ok(oid),
        Some(RawRef::Symbolic(target)) => resolve_via_raw(store, &target, depth + 1),
    }
}
