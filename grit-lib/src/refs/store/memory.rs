//! In-memory [`super::RefStore`] for embedders and tests.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Mutex;

use crate::diff::zero_oid;
use crate::error::Result;

type StoreResult<T> = std::result::Result<T, RefStoreError>;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

use super::error::RefStoreError;
use super::transaction::{expected_matches, RefTransaction};
use super::validation::verify_create_conflicts;
use super::{
    PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefUpdate,
    RefUpdateFlags, ReflogUpdate,
};

#[derive(Debug)]
struct MemoryRefStoreInner {
    refs: BTreeMap<String, RawRef>,
    reflogs: BTreeMap<String, Vec<ReflogEntry>>,
    prepared: Option<PreparedBatch>,
}

#[derive(Debug)]
struct PreparedBatch {
    lock_name: String,
    updates: Vec<RefUpdate>,
}

/// Fully in-memory ref database with transactional updates.
#[derive(Debug)]
pub struct MemoryRefStore {
    inner: Mutex<MemoryRefStoreInner>,
}

impl Default for MemoryRefStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryRefStore {
    /// Create an empty store.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(MemoryRefStoreInner {
                refs: BTreeMap::new(),
                reflogs: BTreeMap::new(),
                prepared: None,
            }),
        }
    }

    /// Seed a ref without going through a transaction (tests and setup).
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::LockHeld`] when a transaction is prepared.
    pub fn set_ref(&self, name: impl Into<String>, value: RawRef) -> StoreResult<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &inner.prepared {
            return Err(RefStoreError::LockHeld {
                name: active.lock_name.clone(),
            });
        }
        inner.refs.insert(name.into(), value);
        Ok(())
    }
}

impl RefStore for MemoryRefStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Memory
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(inner.refs.get(name).cloned())
    }

    fn resolve(&self, name: &str) -> Result<ObjectId> {
        super::resolve_store(self, name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (name, value) in inner.refs.range(prefix.to_string()..) {
            if !name.starts_with(prefix) {
                break;
            }
            let peeled = match &value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(_) => resolve_raw(self, name, 0).ok(),
            };
            let entry = RefEntry {
                name: name.clone(),
                value: value.clone(),
                peeled,
            };
            if f(&entry).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        RefTransaction::validate_no_duplicates(txn.updates())?;
        let updates = txn.into_updates();
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &inner.prepared {
            return Err(RefStoreError::LockHeld {
                name: active.lock_name.clone(),
            });
        }

        for update in &updates {
            let actual = inner.refs.get(&update.name);
            if !expected_matches(actual, &update.expected) {
                return Err(RefStoreError::ExpectedMismatch {
                    name: update.name.clone(),
                    expected: update.expected.clone(),
                    actual: actual.cloned(),
                });
            }
        }

        verify_create_conflicts(&inner.refs, &updates)?;

        let lock_name = updates
            .first()
            .map(|u| u.name.clone())
            .unwrap_or_else(|| "refs".to_owned());
        inner.prepared = Some(PreparedBatch { lock_name, updates });

        Ok(Box::new(MemoryPrepared {
            store: self,
            committed: false,
        }))
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(inner
            .reflogs
            .get(name)
            .is_some_and(|entries| !entries.is_empty()))
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = inner.reflogs.get(name) else {
            return Ok(());
        };
        if reverse {
            for entry in entries.iter().rev() {
                if f(entry).is_break() {
                    break;
                }
            }
        } else {
            for entry in entries {
                if f(entry).is_break() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.reflogs.entry(name.to_owned()).or_default();
        Ok(())
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.reflogs.remove(name);
        Ok(())
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if entries.is_empty() {
            inner.reflogs.remove(name);
        } else {
            inner.reflogs.insert(name.to_owned(), entries);
        }
        Ok(())
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for name in inner.reflogs.keys() {
            if f(name).is_break() {
                break;
            }
        }
        Ok(())
    }
}

struct MemoryPrepared<'a> {
    store: &'a MemoryRefStore,
    committed: bool,
}

impl PreparedRefTransaction for MemoryPrepared<'_> {
    fn commit(mut self: Box<Self>) -> StoreResult<()> {
        let mut inner = self.store.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(batch) = inner.prepared.take() else {
            return Ok(());
        };
        for update in batch.updates {
            apply_update(&mut inner, &update)?;
        }
        self.committed = true;
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> StoreResult<()> {
        let mut inner = self.store.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.prepared = None;
        self.committed = true;
        Ok(())
    }
}

impl Drop for MemoryPrepared<'_> {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(mut inner) = self.store.inner.lock() {
            inner.prepared = None;
        }
    }
}

fn apply_update(inner: &mut MemoryRefStoreInner, update: &RefUpdate) -> StoreResult<()> {
    if !update.flags.log_only {
        match &update.new_value {
            None => {
                inner.refs.remove(&update.name);
            }
            Some(value) => {
                inner.refs.insert(update.name.clone(), value.clone());
            }
        }
    }

    if let Some(log) = &update.reflog {
        let old_oid = reflog_old_oid(inner, &update.name, update.flags);
        let new_oid = reflog_new_oid(inner, update, update.flags);
        append_reflog_entry(inner, &update.name, old_oid, new_oid, log);
    }
    Ok(())
}

fn append_reflog_entry(
    inner: &mut MemoryRefStoreInner,
    name: &str,
    old_oid: ObjectId,
    new_oid: ObjectId,
    log: &ReflogUpdate,
) {
    let identity = format_reflog_identity(&log.identity, log.time);
    let entry = ReflogEntry {
        old_oid,
        new_oid,
        identity,
        message: log.message.clone(),
    };
    inner
        .reflogs
        .entry(name.to_owned())
        .or_default()
        .push(entry);
}

fn format_reflog_identity(identity: &str, time: time::OffsetDateTime) -> String {
    if identity.chars().any(|c| c.is_ascii_digit()) {
        return identity.to_owned();
    }
    let offset = time.offset().whole_seconds();
    let hours = offset / 3600;
    let minutes = (offset.abs() % 3600) / 60;
    format!(
        "{identity} {} {:+03}{:02}",
        time.unix_timestamp(),
        hours,
        minutes
    )
}

fn reflog_old_oid(inner: &MemoryRefStoreInner, name: &str, flags: RefUpdateFlags) -> ObjectId {
    let Some(current) = inner.refs.get(name) else {
        return zero_oid();
    };
    oid_for_reflog(current, flags, |n| resolve_stored(inner, n)).unwrap_or_else(|_| zero_oid())
}

fn reflog_new_oid(
    inner: &MemoryRefStoreInner,
    update: &RefUpdate,
    flags: RefUpdateFlags,
) -> ObjectId {
    if update.flags.log_only {
        return reflog_old_oid(inner, &update.name, flags);
    }
    match &update.new_value {
        None => zero_oid(),
        Some(value) => oid_for_reflog(value, flags, |n| resolve_stored(inner, n))
            .unwrap_or_else(|_| zero_oid()),
    }
}

fn oid_for_reflog(
    value: &RawRef,
    flags: RefUpdateFlags,
    mut resolve_name: impl FnMut(&str) -> StoreResult<ObjectId>,
) -> StoreResult<ObjectId> {
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) if flags.no_deref => Ok(zero_oid()),
        RawRef::Symbolic(target) => resolve_name(target),
    }
}

fn resolve_stored(inner: &MemoryRefStoreInner, name: &str) -> StoreResult<ObjectId> {
    resolve_map(&inner.refs, name, 0)
}

fn resolve_map(refs: &BTreeMap<String, RawRef>, name: &str, depth: usize) -> StoreResult<ObjectId> {
    if depth >= SYMREF_MAXDEPTH {
        return Err(RefStoreError::SymrefLoop);
    }
    let Some(value) = refs.get(name) else {
        return Err(RefStoreError::Corrupt(format!("ref not found: {name}")));
    };
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) => resolve_map(refs, target, depth + 1),
    }
}

fn resolve_raw(store: &MemoryRefStore, name: &str, depth: usize) -> StoreResult<ObjectId> {
    if depth >= SYMREF_MAXDEPTH {
        return Err(RefStoreError::SymrefLoop);
    }
    let inner = store.inner.lock().unwrap_or_else(|e| e.into_inner());
    let Some(value) = inner.refs.get(name) else {
        return Err(RefStoreError::Corrupt(format!("ref not found: {name}")));
    };
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) => {
            let target = target.clone();
            resolve_raw(store, &target, depth + 1)
        }
    }
}
