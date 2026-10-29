//! The ref store a [`crate::repo::Repository`] hands out: its backend, plus checks against
//! the repository's objects before every transaction.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::hooks::RefHookContext;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::reflog::ReflogEntry;

type StoreResult<T> = std::result::Result<T, RefStoreError>;

use super::backend::verify_direct_targets;
use super::reference_transaction::prepare_with_reference_transaction_hooks;
use super::{
    FilesRefStore, PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore,
    RefStoreError, RefTransaction, RefUpdate,
};

/// Check `updates` against `odb`: direct values and explicit reflog object ids must name
/// stored objects; a branch or `HEAD` must name a commit (see [`verify_direct_targets`]).
///
/// # Errors
///
/// [`RefStoreError::MissingObject`], [`RefStoreError::NotACommit`] or
/// [`RefStoreError::ObjectLookup`] for the first offending update.
pub(crate) fn verify_update_targets(odb: &Odb, updates: &[RefUpdate]) -> StoreResult<()> {
    odb.with_pack_read_context(|| verify_direct_targets(updates, |oid| odb.stored_object_kind(oid)))
}

/// The ref store for `git_dir` (as [`super::open_ref_store`] selects it), checked against the
/// objects in that repository's common `objects/` directory, for writers that start from a
/// path rather than a [`crate::repo::Repository`].
///
/// # Errors
///
/// Propagates [`super::open_ref_store`] failures.
pub(crate) fn open_checked_ref_store(git_dir: &Path) -> Result<ObjectCheckedRefStore> {
    let common_dir = crate::refs::common_dir(git_dir).unwrap_or_else(|| git_dir.to_path_buf());
    let odb = Odb::new(&common_dir.join("objects")).with_config_git_dir(git_dir.to_path_buf());
    Ok(ObjectCheckedRefStore::new(
        super::open_ref_store(git_dir)?,
        odb,
    ))
}

/// A repository's ref backend wrapped so [`RefStore::prepare`] and
/// [`RefStore::verify_update_targets`] check updates against the repository's object
/// database.
///
/// Reads, iteration and reflog maintenance go straight to the backend.
pub(crate) struct ObjectCheckedRefStore {
    inner: Arc<dyn RefStore>,
    odb: Odb,
    hooks: Option<Arc<RefHookContext>>,
}

impl ObjectCheckedRefStore {
    /// Wrap `inner`, checking transactions against `odb`.
    pub(crate) fn new(inner: Arc<dyn RefStore>, odb: Odb) -> Self {
        Self {
            inner,
            odb,
            hooks: None,
        }
    }

    /// Install hook context for [`super::reference_transaction`] events.
    pub(crate) fn with_ref_hooks(mut self, hooks: Arc<RefHookContext>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    /// The wrapped backend.
    pub(crate) fn inner(&self) -> &Arc<dyn RefStore> {
        &self.inner
    }

    /// Hash algorithm for null oids in hook stdin.
    pub(crate) fn null_oid(&self) -> ObjectId {
        self.hooks
            .as_ref()
            .map(|ctx| ctx.null_oid())
            .unwrap_or_else(|| ObjectId::null(self.odb.hash_algo()))
    }
}

impl std::fmt::Debug for ObjectCheckedRefStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ObjectCheckedRefStore")
            .field("inner", &self.inner)
            .finish_non_exhaustive()
    }
}

impl RefStore for ObjectCheckedRefStore {
    fn format(&self) -> RefStorageFormat {
        self.inner.format()
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        self.inner.read_raw(name)
    }

    fn resolve(&self, name: &str) -> Result<ObjectId> {
        self.inner.resolve(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.inner.for_each_ref(prefix, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        txn.validate()?;
        self.verify_update_targets(txn.updates())?;
        prepare_with_reference_transaction_hooks(
            self,
            self.inner.as_ref(),
            self.hooks.as_ref(),
            self.null_oid(),
            txn,
        )
    }

    fn verify_update_targets(&self, updates: &[RefUpdate]) -> StoreResult<()> {
        verify_update_targets(&self.odb, updates)
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        self.inner.reflog_exists(name)
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.inner.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        self.inner.create_reflog(name)
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        self.inner.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        self.inner.replace_reflog(name, entries)
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        self.inner.for_each_reflog_ref(f)
    }

    fn as_files(&self) -> Option<&FilesRefStore> {
        self.inner.as_files()
    }

    fn ref_hook_context(&self) -> Option<&RefHookContext> {
        self.hooks.as_deref()
    }

    fn reference_transaction_null_oid(&self) -> ObjectId {
        self.null_oid()
    }
}
