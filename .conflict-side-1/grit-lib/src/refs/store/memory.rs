//! In-memory [`super::RefStore`] for embedders and tests.

use std::ops::ControlFlow;
use std::sync::Mutex;

use crate::error::Result;

type StoreResult<T> = std::result::Result<T, RefStoreError>;
use crate::reflog::ReflogEntry;

use super::apply::{apply_update, resolve_map, simulate_batch_apply, RefBatchState};
use super::error::RefStoreError;
use super::transaction::RefTransaction;
use super::validation::verify_create_conflicts;
use super::{PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefUpdate};

#[derive(Debug, Clone)]
struct MemoryRefStoreInner {
    state: RefBatchState,
    prepared: Option<PreparedBatch>,
}

impl MemoryRefStoreInner {
    fn snapshot_for_apply(&self) -> RefBatchState {
        self.state.snapshot()
    }
}

#[derive(Debug, Clone)]
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
                state: RefBatchState::default(),
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
        inner.state.refs.insert(name.into(), value);
        Ok(())
    }
}

impl RefStore for MemoryRefStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Memory
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        Ok(inner.state.refs.get(name).cloned())
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for (name, value) in inner.state.refs.range(prefix.to_string()..) {
            if !name.starts_with(prefix) {
                break;
            }
            let peeled = match &value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(_) => resolve_map(&inner.state.refs, name, 0).ok(),
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

        verify_create_conflicts(&inner.state.refs, &updates)?;
        simulate_batch_apply(&inner.state, &updates)?;

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
        Ok(inner.state.reflogs.contains_key(name))
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let Some(entries) = inner.state.reflogs.get(name) else {
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
        inner.state.reflogs.entry(name.to_owned()).or_default();
        Ok(())
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.reflogs.remove(name);
        Ok(())
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.state.reflogs.insert(name.to_owned(), entries);
        Ok(())
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        for name in inner.state.reflogs.keys() {
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
        let mut trial = inner.snapshot_for_apply();
        for update in &batch.updates {
            apply_update(&mut trial, update)?;
        }
        inner.state = trial;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectId;
    use crate::refs::store::RefTransaction;

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
