//! The ref store a [`crate::repo::Repository`] hands out: its backend, plus checks against
//! the repository's objects before every transaction.

use std::ops::ControlFlow;
use std::path::Path;
use std::sync::Arc;

use crate::error::Result;
use crate::hooks::RefHookContext;
use crate::lockfile::LockWait;
use crate::objects::ObjectId;
use crate::odb::Odb;
use crate::reflog::ReflogEntry;

type StoreResult<T> = std::result::Result<T, RefStoreError>;
type ObjectPartitionResult = (Vec<RefUpdate>, Vec<(String, RefStoreError)>);

use super::backend::{is_per_ref_rejectable, verify_direct_targets};
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

fn partition_object_target_failures(
    odb: &Odb,
    updates: &[RefUpdate],
) -> StoreResult<ObjectPartitionResult> {
    let mut accepted = Vec::with_capacity(updates.len());
    let mut rejected = Vec::new();
    for update in updates {
        match verify_update_targets(odb, std::slice::from_ref(update)) {
            Ok(()) => accepted.push(update.clone()),
            Err(err) if is_per_ref_rejectable(&err) => rejected.push((update.name.clone(), err)),
            Err(err) => return Err(err),
        }
    }
    Ok((accepted, rejected))
}

fn txn_with_accepted_updates(
    txn: &RefTransaction,
    accepted: Vec<RefUpdate>,
) -> StoreResult<RefTransaction> {
    let mut filtered = RefTransaction::from_updates(accepted)?;
    if txn.is_initial() {
        filtered = filtered.initial();
    }
    if txn.allows_failures() {
        filtered = filtered.allowing_failures();
    }
    for rename in txn.reflog_renames() {
        filtered = filtered.with_reflog_rename(rename.clone());
    }
    for rename in txn.case_only_renames() {
        filtered = filtered.with_case_only_rename(rename.clone());
    }
    Ok(filtered)
}

struct ObjectCheckedPrepared<'a> {
    rejections: Vec<(String, RefStoreError)>,
    inner: Option<Box<dyn PreparedRefTransaction + Send + 'a>>,
}

impl PreparedRefTransaction for ObjectCheckedPrepared<'_> {
    fn rejected_updates(&self) -> &[(String, RefStoreError)] {
        &self.rejections
    }

    fn commit(mut self: Box<Self>) -> StoreResult<()> {
        if let Some(inner) = self.inner.take() {
            inner.commit()?;
        }
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> StoreResult<()> {
        if let Some(inner) = self.inner.take() {
            inner.abort()?;
        }
        Ok(())
    }
}

fn wrap_checked_prepared<'a>(
    object_rejections: Vec<(String, RefStoreError)>,
    inner: Box<dyn PreparedRefTransaction + Send + 'a>,
) -> Box<dyn PreparedRefTransaction + Send + 'a> {
    let mut rejections = object_rejections;
    rejections.extend(
        inner
            .rejected_updates()
            .iter()
            .map(|(name, err)| (name.clone(), err.clone())),
    );
    Box::new(ObjectCheckedPrepared {
        rejections,
        inner: Some(inner),
    })
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
        let (txn, object_rejections) = if txn.allows_failures() {
            let (accepted, object_rejections) =
                partition_object_target_failures(&self.odb, txn.updates())?;
            let txn = txn_with_accepted_updates(&txn, accepted)?;
            (txn, object_rejections)
        } else {
            self.verify_update_targets(txn.updates())?;
            (txn, Vec::new())
        };
        if txn.allows_failures() && txn.updates().is_empty() {
            return Ok(Box::new(ObjectCheckedPrepared {
                rejections: object_rejections,
                inner: None,
            }));
        }
        let prepared = prepare_with_reference_transaction_hooks(
            self,
            self.inner.as_ref(),
            self.hooks.as_ref(),
            self.null_oid(),
            txn,
        )?;
        if object_rejections.is_empty() {
            Ok(prepared)
        } else {
            Ok(wrap_checked_prepared(object_rejections, prepared))
        }
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

    fn rewrite_reflog(
        &self,
        name: &str,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> Result<Vec<ReflogEntry>>,
    ) -> Result<()> {
        self.inner.rewrite_reflog(name, transform)
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

    fn ref_lock_wait(&self) -> LockWait {
        if let Some(ctx) = self.hooks.as_ref() {
            return LockWait::files_ref_lock_for_git_dir(ctx.git_dir());
        }
        if let Some(files) = self.inner.as_files() {
            return LockWait::files_ref_lock_for_git_dir(files.git_dir());
        }
        if let Some(git_dir) = self.odb.config_git_dir() {
            return LockWait::files_ref_lock_for_git_dir(git_dir);
        }
        LockWait::files_ref_lock(None)
    }
}
