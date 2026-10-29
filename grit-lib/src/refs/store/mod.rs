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
mod checked;
mod error;
mod files;
mod linked_worktree;
mod memory;
mod open;
pub mod paths;
pub(crate) mod reference_transaction;
mod reftable;
mod routing;
mod semantics;
mod transaction;
mod types;
mod validation;

pub(crate) use checked::{open_checked_ref_store, ObjectCheckedRefStore};
pub use error::{BackendError, RefStoreError};
pub use files::{FilesRefStore, FilesRefStoreConfig};
pub(crate) use linked_worktree::LinkedWorktreeRefStore;
pub use memory::MemoryRefStore;
pub use open::open_ref_store;
pub(crate) use open::{initialize_head, open_files_ref_store, open_ref_store_uncached};
pub use reftable::ReftableRefStore;
pub(crate) use semantics::format_reflog_identity;
pub use transaction::RefTransaction;
pub use types::{
    Expected, RawRef, RefCaseOnlyRename, RefEntry, RefName, RefReflogRename, RefStorageFormat,
    RefUpdate, RefUpdateFlags, ReflogUpdate,
};
pub(crate) use validation::{
    validate_reflog_identity_string, validate_reflog_message, validate_reflog_update,
};

use std::fmt::Debug;
use std::ops::ControlFlow;

use crate::error::Result;
use crate::lockfile::LockWait;
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

    /// Updates [`RefTransaction::allowing_failures`] skipped during [`Self::commit`]'s prepare.
    ///
    /// Empty when the transaction was not prepared with [`RefTransaction::allowing_failures`].
    fn rejected_updates(&self) -> &[(String, RefStoreError)] {
        &[]
    }

    /// Release locks without changing stored refs.
    ///
    /// # Errors
    ///
    /// Propagates backend errors while releasing resources.
    fn abort(self: Box<Self>) -> StoreResult<()>;
}

/// Reference storage backend (loose/packed files, reftable, or in-memory).
///
/// # Re-entrant callbacks
///
/// The callbacks passed to [`Self::for_each_ref`], [`Self::for_each_reflog_entry`] and
/// [`Self::for_each_reflog_ref`] may call back into the same store, both to read (resolve a
/// symref, read each listed ref's reflog) and to write. The set of names to visit is fixed when
/// the walk starts, so a write made from the callback may not be seen by the walk in progress.
/// Implementations must not hold internal locks while a callback runs.
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
    ///
    /// A non-empty `prefix` is a literal name prefix, so `"H"` or `"HEAD"` lists `HEAD` and
    /// `"ORIG"` lists `ORIG_HEAD`. An empty `prefix` lists only the refs under `refs/`, like
    /// `git for-each-ref` with no pattern: root refs such as `HEAD` and `ORIG_HEAD` are left
    /// out and stay reachable through [`Self::read_raw`] or a non-empty prefix they match.
    ///
    /// `f` may call back into the store; see [re-entrant callbacks](Self#re-entrant-callbacks).
    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()>;

    /// Walk refs stored in this handle's physical reftable stack (`reftable/` under the
    /// store git directory). On the files backend this matches [`Self::for_each_ref`].
    fn for_each_physical_stack_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.for_each_ref(prefix, f)
    }

    /// Validate `txn`, take locks, and return a committable batch.
    ///
    /// Every check runs here, before anything is written, and a failure leaves the store
    /// unchanged and unlocked. A store must:
    ///
    /// 1. Before taking locks, reject names Git refuses to store (an update name or either
    ///    side of a reflog or case-only rename) with [`RefStoreError::InvalidRefName`], a
    ///    symbolic value Git refuses with [`RefStoreError::InvalidSymrefTarget`], an update
    ///    other than a reflog-only one that would store the null object id with
    ///    [`RefStoreError::NullOid`], and malformed reflog identity or message fields with
    ///    [`RefStoreError::InvalidReflog`].
    /// 2. While another writer holds a lock this transaction needs, fail promptly with
    ///    [`RefStoreError::LockHeld`] naming this transaction's first ref. Do not spin or
    ///    queue: grit-lib's ref writes go through [`crate::refs::prepare_with_lock_wait`],
    ///    which retries with backoff for `core.filesRefLockTimeout`. The files backend takes
    ///    one lock per ref (plus packed-refs when needed), so only the same ref contends.
    ///    A store with one writer slot for everything may report every overlap this way, so
    ///    disjoint refs serialize but still all succeed.
    /// 3. Reject a created ref whose name is a directory prefix of a stored ref or of
    ///    another ref in the batch, or the other way round (`a` vs `a/b`), with
    ///    [`RefStoreError::NameUnavailable`]. Deletes in the same batch do not free a name.
    /// 4. Check each [`RefUpdate::expected`] against the state left by the earlier updates
    ///    in the batch ([`RefStoreError::ExpectedMismatch`]). For a direct-value update
    ///    through a symbolic ref, an [`Expected::Oid`] is compared with the resolved id.
    /// 5. Reject two updates that write the same stored ref, including through symbolic
    ///    refs, with [`RefStoreError::DuplicateUpdate`].
    ///
    /// [`backend::check_transaction`] performs steps 1 and 3–5 and
    /// [`backend::apply_transaction`] defines what [`PreparedRefTransaction::commit`] stores,
    /// including reflog lines.
    ///
    /// A backend does not look at objects. The store
    /// [`Repository::refs`](crate::repo::Repository::refs) returns (whatever backend it
    /// wraps, including one injected with
    /// [`RepositoryBuilder::ref_store`](crate::repo::RepositoryBuilder::ref_store)) adds one
    /// more check before calling the backend. Each direct value must name an object the
    /// repository stores ([`RefStoreError::MissingObject`]). A branch (`refs/heads/…`) or
    /// `HEAD` must name a commit ([`RefStoreError::NotACommit`]). An object that cannot be
    /// read fails with [`RefStoreError::ObjectLookup`]. Deletes and symbolic values are not
    /// looked up. Reflog-only updates and explicit reflog object ids are checked like direct
    /// values. Updates flagged [`RefUpdateFlags::skip_oid_verification`] are not looked up.
    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>>;

    /// Check the objects `updates` would point refs at, without taking any lock or writing.
    ///
    /// This is the object check [`Self::prepare`] makes on the store
    /// [`Repository::refs`](crate::repo::Repository::refs) returns. Writers that update a ref
    /// without a transaction (the files-backend fast path behind
    /// [`update_branch_for_commit_on_store`](crate::refs::update_branch_for_commit_on_store))
    /// call it before locking. Backends do not look at objects, so the default accepts
    /// everything; there is no need to override it.
    ///
    /// # Errors
    ///
    /// On the repository's store: [`RefStoreError::MissingObject`],
    /// [`RefStoreError::NotACommit`] or [`RefStoreError::ObjectLookup`] for the first
    /// offending update.
    fn verify_update_targets(&self, updates: &[RefUpdate]) -> StoreResult<()> {
        let _ = updates;
        Ok(())
    }

    /// Hook context when this store runs `reference-transaction` hooks.
    #[doc(hidden)]
    fn ref_hook_context(&self) -> Option<&crate::hooks::RefHookContext> {
        let _ = self;
        None
    }

    /// Null object id width for `reference-transaction` stdin on this store.
    #[doc(hidden)]
    fn reference_transaction_null_oid(&self) -> ObjectId {
        crate::diff::zero_oid()
    }

    /// Whether a reflog is present for `name`.
    fn reflog_exists(&self, name: &str) -> Result<bool>;

    /// Iterate reflog entries for `name` (oldest first unless `reverse`).
    ///
    /// `f` may call back into the store; see [re-entrant callbacks](Self#re-entrant-callbacks).
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

    /// Read a reflog under the backend writer lock, transform it, and write it back atomically.
    ///
    /// The closure must not call back into this store. Implementations hold the per-ref lock
    /// (files backend) or an equivalent writer lock for the whole read-transform-write.
    ///
    /// The default reads entries, runs `transform`, and calls [`Self::replace_reflog`], which is
    /// not safe under concurrent ref updates; built-in backends override this method.
    ///
    /// # Errors
    ///
    /// Propagates lock, I/O, and transform failures. [`RefStoreError::ReflogRewriteStale`] is
    /// returned when a caller compares against a snapshot and the locked re-read no longer
    /// matches (retry the outer operation).
    fn rewrite_reflog(
        &self,
        name: &str,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> Result<Vec<ReflogEntry>>,
    ) -> Result<()> {
        let mut entries = Vec::new();
        self.for_each_reflog_entry(name, false, &mut |entry| {
            entries.push(entry.clone());
            ControlFlow::Continue(())
        })?;
        let rewritten = transform(entries)?;
        self.replace_reflog(name, rewritten)
    }

    /// Invoke `f` for each ref name that has a reflog, in sorted order.
    ///
    /// `f` may call back into the store; see [re-entrant callbacks](Self#re-entrant-callbacks).
    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()>;

    /// The files backend behind this store, when it is one.
    ///
    /// Lets callers reuse state the files backend keeps between calls (such as its
    /// `packed-refs` snapshot) instead of reading the same files again. Other backends,
    /// including injected ones, return `None`.
    fn as_files(&self) -> Option<&FilesRefStore> {
        None
    }

    /// How long grit-lib retries [`RefStoreError::LockHeld`] from [`Self::prepare`].
    ///
    /// The files backend reads `core.filesRefLockTimeout` from its git directory. Injected
    /// backends return Git's 100 ms default unless a wrapper (such as the store returned by
    /// [`crate::repo::Repository::refs`]) overrides this from the repository git directory.
    fn ref_lock_wait(&self) -> LockWait {
        self.as_files()
            .map(|files| LockWait::files_ref_lock_for_git_dir(files.git_dir()))
            .unwrap_or_else(|| LockWait::files_ref_lock(None))
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
