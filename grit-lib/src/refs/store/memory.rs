//! In-memory [`super::RefStore`] for embedders and tests.

use std::ops::ControlFlow;
use std::sync::Mutex;

use crate::error::Result;

type StoreResult<T> = std::result::Result<T, RefStoreError>;
use crate::reflog::ReflogEntry;

use super::backend::{apply_transaction, check_transaction, resolve_symbolic, RefBatchState};
use super::error::RefStoreError;
use super::transaction::RefTransaction;
use super::{PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore};

#[derive(Debug, Clone)]
struct MemoryRefStoreInner {
    state: RefBatchState,
    prepared: Option<PreparedBatch>,
}

#[derive(Debug, Clone)]
struct PreparedBatch {
    lock_name: String,
    txn: RefTransaction,
}

/// Fully in-memory ref database with transactional updates.
///
/// Built only on the public [`super::backend`] helpers, so it doubles as a reference for
/// custom backends.
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
                RawRef::Symbolic(_) => resolve_symbolic(&inner.state.refs, name).ok(),
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
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(active) = &inner.prepared {
            return Err(RefStoreError::LockHeld {
                name: active.lock_name.clone(),
            });
        }

        check_transaction(&inner.state, &txn)?;

        let lock_name = txn
            .updates()
            .first()
            .map(|u| u.name.clone())
            .or_else(|| txn.reflog_renames().first().map(|r| r.from.clone()))
            .or_else(|| txn.case_only_renames().first().map(|r| r.from.clone()))
            .unwrap_or_else(|| "refs".to_owned());
        inner.prepared = Some(PreparedBatch { lock_name, txn });

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
        let mut trial = inner.state.snapshot();
        apply_transaction(&mut trial, &batch.txn)?;
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
        let mut inner = self.store.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.prepared = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectId;

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
