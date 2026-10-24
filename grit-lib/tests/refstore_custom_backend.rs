//! A ref backend written outside grit-lib, using only the public `refs::store::backend`
//! helpers, passes the same conformance suite as the built-in stores.

use std::fmt;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use grit_lib::error::{Error, Result};
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::backend::{
    apply_transaction, check_transaction, resolve_symbolic, RefBatchState,
};
use grit_lib::refs::store::{
    BackendError, PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore,
    RefStoreError, RefTransaction,
};
use grit_test_support::refstore_conformance::run_suite;

#[derive(Debug, Default)]
struct Inner {
    state: RefBatchState,
    locked: Option<RefTransaction>,
}

/// Map-backed store with a single transaction lock, like a database row lock.
#[derive(Debug, Default)]
struct MapStore {
    inner: Mutex<Inner>,
}

impl MapStore {
    fn inner(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl RefStore for MapStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Custom("map")
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        Ok(self.inner().state.refs.get(name).cloned())
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let inner = self.inner();
        for (name, value) in inner.state.refs.range(prefix.to_owned()..) {
            if !name.starts_with(prefix) {
                break;
            }
            let peeled = match value {
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
    ) -> std::result::Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        let mut inner = self.inner();
        if inner.locked.is_some() {
            return Err(RefStoreError::LockHeld {
                name: "refs".to_owned(),
            });
        }
        check_transaction(&inner.state, &txn)?;
        inner.locked = Some(txn);
        Ok(Box::new(MapPrepared {
            store: self,
            done: false,
        }))
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        Ok(self.inner().state.reflogs.contains_key(name))
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let inner = self.inner();
        let Some(entries) = inner.state.reflogs.get(name) else {
            return Ok(());
        };
        let mut visit = |entry: &ReflogEntry| f(entry).is_break();
        if reverse {
            entries.iter().rev().any(&mut visit);
        } else {
            entries.iter().any(&mut visit);
        }
        Ok(())
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        self.inner()
            .state
            .reflogs
            .entry(name.to_owned())
            .or_default();
        Ok(())
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        self.inner().state.reflogs.remove(name);
        Ok(())
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        self.inner().state.reflogs.insert(name.to_owned(), entries);
        Ok(())
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let inner = self.inner();
        inner.state.reflogs.keys().any(|name| f(name).is_break());
        Ok(())
    }
}

struct MapPrepared<'a> {
    store: &'a MapStore,
    done: bool,
}

impl PreparedRefTransaction for MapPrepared<'_> {
    fn commit(mut self: Box<Self>) -> std::result::Result<(), RefStoreError> {
        self.done = true;
        let mut inner = self.store.inner();
        let Some(txn) = inner.locked.take() else {
            return Ok(());
        };
        let mut next = inner.state.clone();
        apply_transaction(&mut next, &txn)?;
        inner.state = next;
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> std::result::Result<(), RefStoreError> {
        self.done = true;
        self.store.inner().locked = None;
        Ok(())
    }
}

impl Drop for MapPrepared<'_> {
    fn drop(&mut self) {
        if !self.done {
            self.store.inner().locked = None;
        }
    }
}

#[test]
fn custom_backend_built_on_public_helpers_passes_conformance_suite() {
    run_suite(|| Arc::new(MapStore::default()) as Arc<dyn RefStore>);
}

#[test]
fn custom_format_reports_its_name() {
    let store = MapStore::default();
    assert_eq!(store.format(), RefStorageFormat::Custom("map"));
    assert_eq!(store.format().to_string(), "map");
}

#[derive(Debug)]
struct ConnectionLost;

impl fmt::Display for ConnectionLost {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("connection to ref database lost")
    }
}

impl std::error::Error for ConnectionLost {}

/// Store whose reads fail with a backend error, to check the error stays typed through
/// [`RefStore::resolve`].
#[derive(Debug)]
struct Offline;

impl RefStore for Offline {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Custom("offline")
    }

    fn read_raw(&self, _name: &str) -> Result<Option<RawRef>> {
        Err(RefStoreError::backend(ConnectionLost).into())
    }

    fn for_each_ref(
        &self,
        _prefix: &str,
        _f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        Err(RefStoreError::backend(ConnectionLost).into())
    }

    fn prepare(
        &self,
        _txn: RefTransaction,
    ) -> std::result::Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        Err(RefStoreError::backend(ConnectionLost))
    }

    fn reflog_exists(&self, _name: &str) -> Result<bool> {
        Ok(false)
    }

    fn for_each_reflog_entry(
        &self,
        _name: &str,
        _reverse: bool,
        _f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        Ok(())
    }

    fn create_reflog(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn delete_reflog(&self, _name: &str) -> Result<()> {
        Ok(())
    }

    fn replace_reflog(&self, _name: &str, _entries: Vec<ReflogEntry>) -> Result<()> {
        Ok(())
    }

    fn for_each_reflog_ref(&self, _f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        Ok(())
    }
}

#[test]
fn backend_errors_stay_typed_and_downcastable() {
    let err = Offline.resolve("HEAD").expect_err("offline");
    let Error::RefStore(RefStoreError::Backend(inner)) = &err else {
        panic!("expected a backend error, got {err:?}");
    };
    assert!(inner.downcast_ref::<ConnectionLost>().is_some());
    assert_eq!(err.to_string(), "connection to ref database lost");

    let prepare_err = match Offline.prepare(RefTransaction::new()) {
        Err(err) => err,
        Ok(_) => panic!("expected prepare to fail"),
    };
    assert!(
        matches!(&prepare_err, RefStoreError::Backend(e) if e.downcast_ref::<ConnectionLost>().is_some())
    );

    let shared = BackendError::new(ConnectionLost);
    let a = RefStoreError::from(shared.clone());
    assert_eq!(a, RefStoreError::Backend(shared));
    assert_ne!(a, RefStoreError::backend(ConnectionLost));
}
