//! In-memory [`super::RefStore`] for embedders and tests.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::error::Result;

type StoreResult<T> = std::result::Result<T, RefStoreError>;
use crate::reflog::ReflogEntry;

use super::apply::{rollback_failed_apply, snapshot_for_apply_rollback};
use super::backend::{apply_transaction, check_transaction, resolve_symbolic, RefBatchState};
use super::error::RefStoreError;
use super::transaction::RefTransaction;
use super::{PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore};

#[derive(Debug, Clone)]
struct MemoryRefStoreInner {
    /// Shared so iterators can walk a snapshot after releasing the lock; writers go through
    /// [`Arc::make_mut`], which copies only while such a walk still holds the old state.
    state: Arc<RefBatchState>,
    /// The single writer slot: the transaction between `prepare` and commit or abort.
    prepared: Option<RefTransaction>,
}

impl MemoryRefStoreInner {
    fn state_mut(&mut self) -> &mut RefBatchState {
        Arc::make_mut(&mut self.state)
    }
}

/// Fully in-memory ref database with transactional updates.
///
/// Built only on the public [`super::backend`] helpers, so it doubles as a reference for
/// custom backends. One prepared transaction at a time holds the whole store; another
/// `prepare` meanwhile fails with [`RefStoreError::LockHeld`], which grit-lib's ref writes
/// retry (see [`crate::refs::prepare_with_lock_wait`]).
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
                state: Arc::default(),
                prepared: None,
            }),
        }
    }

    /// Seed a ref without going through a transaction (tests and setup).
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::LockHeld`] naming `name` when a transaction is prepared.
    pub fn set_ref(&self, name: impl Into<String>, value: RawRef) -> StoreResult<()> {
        let name = name.into();
        let mut inner = self.lock();
        if inner.prepared.is_some() {
            return Err(RefStoreError::LockHeld { name });
        }
        inner.state_mut().refs.insert(name, value);
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, MemoryRefStoreInner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The current state, detached from the lock so iteration callbacks can re-enter the store.
    fn snapshot(&self) -> Arc<RefBatchState> {
        Arc::clone(&self.lock().state)
    }
}

impl RefStore for MemoryRefStore {
    fn format(&self) -> RefStorageFormat {
        RefStorageFormat::Memory
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        Ok(self.lock().state.refs.get(name).cloned())
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
            let peeled = match &value {
                RawRef::Direct(_) => None,
                RawRef::Symbolic(_) => resolve_symbolic(&state.refs, name).ok(),
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
        txn.validate()?;
        let mut inner = self.lock();
        if inner.prepared.is_some() {
            return Err(RefStoreError::LockHeld {
                name: txn.lock_name(),
            });
        }

        check_transaction(&inner.state, &txn)?;
        inner.prepared = Some(txn);

        Ok(Box::new(MemoryPrepared {
            store: self,
            committed: false,
        }))
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        Ok(self.lock().state.reflogs.contains_key(name))
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        let state = self.snapshot();
        let Some(entries) = state.reflogs.get(name) else {
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
        let mut inner = self.lock();
        if !inner.state.reflogs.contains_key(name) {
            inner
                .state_mut()
                .reflogs
                .insert(name.to_owned(), Vec::new());
        }
        Ok(())
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        let mut inner = self.lock();
        if inner.state.reflogs.contains_key(name) {
            inner.state_mut().reflogs.remove(name);
        }
        Ok(())
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        self.lock()
            .state_mut()
            .reflogs
            .insert(name.to_owned(), entries);
        Ok(())
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let state = self.snapshot();
        for name in state.reflogs.keys() {
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
        let mut inner = self.store.lock();
        let Some(txn) = inner.prepared.take() else {
            return Ok(());
        };
        let backup = snapshot_for_apply_rollback(&inner.state, &txn);
        let state = inner.state_mut();
        if let Err(err) = apply_transaction(state, &txn) {
            rollback_failed_apply(state, &backup);
            return Err(err);
        }
        self.committed = true;
        Ok(())
    }

    fn abort(mut self: Box<Self>) -> StoreResult<()> {
        let mut inner = self.store.lock();
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

    /// Run `body` on its own thread and fail if it does not finish, so a callback that
    /// deadlocks on the store's lock fails the test instead of hanging the suite.
    fn finishes_promptly(body: impl FnOnce() + Send + 'static) {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            body();
            let _ = done_tx.send(());
        });
        match done_rx.recv_timeout(std::time::Duration::from_secs(10)) {
            Ok(()) => handle.join().expect("test thread"),
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if let Err(panic) = handle.join() {
                    std::panic::resume_unwind(panic);
                }
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                panic!("callback re-entering the store deadlocked")
            }
        }
    }

    fn sample_reflog(byte: u8) -> Vec<ReflogEntry> {
        vec![ReflogEntry {
            old_oid: oid(0),
            new_oid: oid(byte),
            identity: "A <a@example.com> 1 +0000".to_owned(),
            message: format!("m{byte}"),
        }]
    }

    #[test]
    fn for_each_ref_callback_can_read_the_store() {
        finishes_promptly(|| {
            let store = MemoryRefStore::new();
            store
                .set_ref("refs/heads/main", RawRef::Direct(oid(1)))
                .expect("main");
            store
                .set_ref(
                    "refs/heads/alias",
                    RawRef::Symbolic("refs/heads/main".into()),
                )
                .expect("alias");

            let mut seen = Vec::new();
            store
                .for_each_ref("refs/", &mut |entry| {
                    let raw = store.read_raw(&entry.name).expect("read_raw");
                    let resolved = store.resolve(&entry.name).expect("resolve");
                    let mut nested = 0usize;
                    store
                        .for_each_ref("refs/heads/", &mut |_| {
                            nested += 1;
                            ControlFlow::Continue(())
                        })
                        .expect("nested for_each_ref");
                    seen.push((entry.name.clone(), raw, resolved, nested));
                    ControlFlow::Continue(())
                })
                .expect("for_each_ref");
            assert_eq!(
                seen,
                vec![
                    (
                        "refs/heads/alias".to_owned(),
                        Some(RawRef::Symbolic("refs/heads/main".into())),
                        oid(1),
                        2,
                    ),
                    (
                        "refs/heads/main".to_owned(),
                        Some(RawRef::Direct(oid(1))),
                        oid(1),
                        2,
                    ),
                ]
            );
        });
    }

    #[test]
    fn for_each_reflog_ref_callback_can_read_each_reflog() {
        finishes_promptly(|| {
            let store = MemoryRefStore::new();
            store
                .replace_reflog("HEAD", sample_reflog(1))
                .expect("HEAD log");
            store
                .replace_reflog("refs/heads/main", sample_reflog(2))
                .expect("main log");

            let mut dump = Vec::new();
            store
                .for_each_reflog_ref(&mut |name| {
                    assert!(store.reflog_exists(name).expect("reflog_exists"));
                    store
                        .for_each_reflog_entry(name, false, &mut |entry| {
                            assert!(store.read_raw(name).expect("read_raw").is_none());
                            dump.push((name.to_owned(), entry.new_oid));
                            ControlFlow::Continue(())
                        })
                        .expect("for_each_reflog_entry");
                    ControlFlow::Continue(())
                })
                .expect("for_each_reflog_ref");
            assert_eq!(
                dump,
                vec![
                    ("HEAD".to_owned(), oid(1)),
                    ("refs/heads/main".to_owned(), oid(2)),
                ]
            );
        });
    }

    #[test]
    fn writes_from_callbacks_succeed_without_changing_the_walk() {
        finishes_promptly(|| {
            let store = MemoryRefStore::new();
            store
                .set_ref("refs/heads/a", RawRef::Direct(oid(1)))
                .expect("a");
            store
                .set_ref("refs/heads/b", RawRef::Direct(oid(2)))
                .expect("b");
            store
                .replace_reflog("refs/heads/a", sample_reflog(1))
                .expect("log");

            let mut names = Vec::new();
            store
                .for_each_ref("refs/heads/", &mut |entry| {
                    store
                        .set_ref("refs/heads/added", RawRef::Direct(oid(3)))
                        .expect("set_ref from callback");
                    names.push(entry.name.clone());
                    ControlFlow::Continue(())
                })
                .expect("for_each_ref");
            assert_eq!(names, vec!["refs/heads/a", "refs/heads/b"]);
            assert_eq!(
                store.read_raw("refs/heads/added").expect("read"),
                Some(RawRef::Direct(oid(3)))
            );

            let mut entries = 0usize;
            store
                .for_each_reflog_entry("refs/heads/a", true, &mut |_| {
                    store
                        .replace_reflog("refs/heads/a", Vec::new())
                        .expect("replace_reflog from callback");
                    entries += 1;
                    ControlFlow::Continue(())
                })
                .expect("for_each_reflog_entry");
            assert_eq!(entries, 1);

            let mut logged = Vec::new();
            store
                .for_each_reflog_ref(&mut |name| {
                    store
                        .delete_reflog(name)
                        .expect("delete_reflog from callback");
                    store.create_reflog("refs/heads/zz").expect("create_reflog");
                    logged.push(name.to_owned());
                    ControlFlow::Continue(())
                })
                .expect("for_each_reflog_ref");
            assert_eq!(logged, vec!["refs/heads/a"]);
            assert!(!store.reflog_exists("refs/heads/a").expect("exists"));
            assert!(store.reflog_exists("refs/heads/zz").expect("exists"));
        });
    }
}
