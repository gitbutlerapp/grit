//! A ref backend written outside grit-lib, using only the public `refs::store::backend`
//! helpers, passes the same conformance suite as the built-in stores.

use grit_lib::error::RefError;
use std::fmt;
use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use grit_lib::error::{Error, Result};
use grit_lib::refs::check_ref_format::RefNamePolicy;
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::backend::{
    apply_transaction, check_transaction, resolve_symbolic, RefBatchState,
};
use grit_lib::refs::store::{
    BackendError, MemoryRefStore, PreparedRefTransaction, RawRef, RefEntry, RefName, RefNameRef,
    RefStorageFormat, RefStore, RefStoreError, RefTransaction,
};

fn rnr(name: &str) -> &RefNameRef {
    RefNameRef::parse(name, &RefNamePolicy::default()).expect("valid test ref name")
}

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

    /// A copy of the state, so iteration callbacks can call back into the store.
    fn snapshot(&self) -> RefBatchState {
        self.inner().state.clone()
    }
}

impl RefStore for MapStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Custom("map")
    }

    fn read_raw(&self, name: &RefNameRef) -> Result<Option<RawRef>> {
        Ok(self.inner().state.refs.get(name.as_str()).cloned())
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let prefix = if prefix.is_empty() { "refs/" } else { prefix };
        let state = self.snapshot();
        for (name, value) in state.refs.range(prefix.to_owned()..) {
            if !name.starts_with(prefix) {
                break;
            }
            let peeled = match value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(_) => resolve_symbolic(&state.refs, name).ok(),
            };
            let entry = RefEntry {
                name: RefName::from_trusted(name.clone()),
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

    fn reflog_exists(&self, name: &RefNameRef) -> Result<bool> {
        Ok(self.inner().state.reflogs.contains_key(name.as_str()))
    }

    fn for_each_reflog_entry(
        &self,
        name: &RefNameRef,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let state = self.snapshot();
        let Some(entries) = state.reflogs.get(name.as_str()) else {
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

    fn create_reflog(&self, name: &RefNameRef) -> Result<()> {
        self.inner()
            .state
            .reflogs
            .entry(name.as_str().to_owned())
            .or_default();
        Ok(())
    }

    fn delete_reflog(&self, name: &RefNameRef) -> Result<()> {
        self.inner().state.reflogs.remove(name.as_str());
        Ok(())
    }

    fn replace_reflog(&self, name: &RefNameRef, entries: Vec<ReflogEntry>) -> Result<()> {
        self.inner()
            .state
            .reflogs
            .insert(name.as_str().to_owned(), entries);
        Ok(())
    }

    fn rewrite_reflog(
        &self,
        name: &RefNameRef,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> Result<Vec<ReflogEntry>>,
    ) -> Result<()> {
        let mut inner = self.inner();
        let current = inner
            .state
            .reflogs
            .get(name.as_str())
            .cloned()
            .unwrap_or_default();
        let rewritten = transform(current)?;
        inner
            .state
            .reflogs
            .insert(name.as_str().to_owned(), rewritten);
        Ok(())
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&RefNameRef) -> ControlFlow<()>) -> Result<()> {
        let state = self.snapshot();
        for key in state.reflogs.keys() {
            if f(RefNameRef::from_trusted(key)).is_break() {
                break;
            }
        }
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
        apply_transaction(&mut inner.state, &txn)?;
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

/// Forwards to [`MemoryRefStore`] but uses an unsafe read/`replace_reflog` rewrite (issue #2395).
#[derive(Debug)]
struct PlainForwarding(MemoryRefStore);

impl RefStore for PlainForwarding {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Custom("plain-forwarding")
    }

    fn read_raw(&self, name: &RefNameRef) -> Result<Option<RawRef>> {
        self.0.read_raw(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.0.for_each_ref(prefix, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> std::result::Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        self.0.prepare(txn)
    }

    fn reflog_exists(&self, name: &RefNameRef) -> Result<bool> {
        self.0.reflog_exists(name)
    }

    fn for_each_reflog_entry(
        &self,
        name: &RefNameRef,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.0.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &RefNameRef) -> Result<()> {
        self.0.create_reflog(name)
    }

    fn delete_reflog(&self, name: &RefNameRef) -> Result<()> {
        self.0.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &RefNameRef, entries: Vec<ReflogEntry>) -> Result<()> {
        self.0.replace_reflog(name, entries)
    }

    fn rewrite_reflog(
        &self,
        name: &RefNameRef,
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

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&RefNameRef) -> ControlFlow<()>) -> Result<()> {
        self.0.for_each_reflog_ref(f)
    }
}

#[test]
fn unsafe_rewrite_reflog_pattern_fails_conformance() {
    use grit_test_support::refstore_conformance::rewrite_reflog_serializes_concurrent_append;
    let store = Arc::new(PlainForwarding(MemoryRefStore::new())) as Arc<dyn RefStore>;
    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        rewrite_reflog_serializes_concurrent_append(store);
    }));
    assert!(
        outcome.is_err(),
        "unsafe rewrite_reflog must fail the concurrent-append conformance case"
    );
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

    fn read_raw(&self, _name: &RefNameRef) -> Result<Option<RawRef>> {
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

    fn reflog_exists(&self, _name: &RefNameRef) -> Result<bool> {
        Ok(false)
    }

    fn for_each_reflog_entry(
        &self,
        _name: &RefNameRef,
        _reverse: bool,
        _f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        Ok(())
    }

    fn create_reflog(&self, _name: &RefNameRef) -> Result<()> {
        Ok(())
    }

    fn delete_reflog(&self, _name: &RefNameRef) -> Result<()> {
        Ok(())
    }

    fn replace_reflog(&self, _name: &RefNameRef, _entries: Vec<ReflogEntry>) -> Result<()> {
        Ok(())
    }

    fn rewrite_reflog(
        &self,
        _name: &RefNameRef,
        _transform: &mut dyn FnMut(Vec<ReflogEntry>) -> Result<Vec<ReflogEntry>>,
    ) -> Result<()> {
        Err(RefStoreError::backend(ConnectionLost).into())
    }

    fn for_each_reflog_ref(
        &self,
        _f: &mut dyn FnMut(&RefNameRef) -> ControlFlow<()>,
    ) -> Result<()> {
        Ok(())
    }
}

#[test]
fn backend_errors_stay_typed_and_downcastable() {
    let err = Offline
        .resolve(RefNameRef::from_trusted("HEAD"))
        .expect_err("offline");
    let Error::Ref(RefError::Store(RefStoreError::Backend(inner))) = &err else {
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
