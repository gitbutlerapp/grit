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
                RawRef::Symbolic(_) => resolve_map(&inner.refs, name, 0).ok(),
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
        Ok(inner.reflogs.contains_key(name))
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
        inner.reflogs.insert(name.to_owned(), entries);
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
    let reflog_oids = update.reflog.as_ref().map(|_| {
        (
            reflog_old_oid_before(inner, update),
            reflog_new_oid_before(inner, update),
        )
    });

    apply_ref_change(inner, update)?;

    if let (Some(log), Some((old_oid, new_oid))) = (&update.reflog, reflog_oids) {
        append_reflog_entry(inner, &update.name, old_oid, new_oid, log);
    }
    Ok(())
}

fn apply_ref_change(inner: &mut MemoryRefStoreInner, update: &RefUpdate) -> StoreResult<()> {
    if update.flags.log_only {
        return Ok(());
    }
    match &update.new_value {
        None => {
            inner.refs.remove(&update.name);
        }
        Some(new_value) => {
            if should_deref_symref_update(inner, update) {
                let RawRef::Direct(oid) = new_value else {
                    return Err(RefStoreError::Corrupt(
                        "deref update requires direct oid".to_owned(),
                    ));
                };
                let target = symref_peel_write_target(&inner.refs, &update.name)?;
                inner.refs.insert(target, RawRef::Direct(*oid));
            } else {
                inner.refs.insert(update.name.clone(), new_value.clone());
            }
        }
    }
    Ok(())
}

fn should_deref_symref_update(inner: &MemoryRefStoreInner, update: &RefUpdate) -> bool {
    if update.flags.no_deref {
        return false;
    }
    matches!(
        (inner.refs.get(&update.name), update.new_value.as_ref()),
        (Some(RawRef::Symbolic(_)), Some(RawRef::Direct(_)))
    )
}

fn symref_peel_write_target(
    refs: &BTreeMap<String, RawRef>,
    sym_name: &str,
) -> StoreResult<String> {
    let Some(RawRef::Symbolic(first)) = refs.get(sym_name) else {
        return Err(RefStoreError::Corrupt(format!(
            "not a symbolic ref: {sym_name}"
        )));
    };
    resolve_symref_peel_target(refs, first)
}

fn resolve_symref_peel_target(
    refs: &BTreeMap<String, RawRef>,
    start: &str,
) -> StoreResult<String> {
    let mut name = start.to_owned();
    let mut depth = 0;
    loop {
        if depth >= SYMREF_MAXDEPTH {
            return Err(RefStoreError::SymrefLoop);
        }
        match refs.get(name.as_str()) {
            Some(RawRef::Direct(_)) => return Ok(name),
            Some(RawRef::Symbolic(next)) => {
                name = next.clone();
                depth += 1;
            }
            None => return Err(RefStoreError::Corrupt(format!("ref not found: {name}"))),
        }
    }
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

fn reflog_old_oid_before(inner: &MemoryRefStoreInner, update: &RefUpdate) -> ObjectId {
    resolved_oid_at_name(inner, &update.name, update.flags).unwrap_or_else(|_| zero_oid())
}

fn reflog_new_oid_before(inner: &MemoryRefStoreInner, update: &RefUpdate) -> ObjectId {
    if update.flags.log_only {
        return reflog_old_oid_before(inner, update);
    }
    match &update.new_value {
        None => zero_oid(),
        Some(RawRef::Direct(oid)) if should_deref_symref_update(inner, update) => *oid,
        Some(value) => {
            oid_for_reflog_value(inner, value, update.flags).unwrap_or_else(|_| zero_oid())
        }
    }
}

fn resolved_oid_at_name(
    inner: &MemoryRefStoreInner,
    name: &str,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    let Some(value) = inner.refs.get(name) else {
        return Ok(zero_oid());
    };
    oid_for_reflog_value(inner, value, flags)
}

fn oid_for_reflog_value(
    inner: &MemoryRefStoreInner,
    value: &RawRef,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) if flags.no_deref => Ok(zero_oid()),
        RawRef::Symbolic(target) => resolve_map(&inner.refs, target, 0),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::store::{Expected, RefTransaction};

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("valid oid")
    }

    #[test]
    fn for_each_ref_peels_symbolic_without_deadlock() {
        let store = MemoryRefStore::new();
        store
            .set_ref("refs/heads/main", RawRef::Direct(oid(1)))
            .expect("main");
        store
            .set_ref("HEAD", RawRef::Symbolic("refs/heads/main".to_owned()))
            .expect("head");

        let mut peeled = None;
        store
            .for_each_ref("HEAD", &mut |entry| {
                peeled = entry.peeled;
                ControlFlow::Break(())
            })
            .expect("iter");
        assert_eq!(peeled, Some(oid(1)));
    }
}
