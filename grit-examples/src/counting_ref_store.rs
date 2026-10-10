//! [`CountingRefStore`]: embedder wrapper that counts ref store operations without logging.

use std::ops::ControlFlow;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use grit_lib::error::Result;
use grit_lib::reflog::ReflogEntry;
use grit_lib::refs::store::{
    PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefStoreError,
    RefTransaction,
};

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// In-memory ref store that forwards to an inner store and counts hot operations.
#[derive(Debug)]
pub struct CountingRefStore {
    inner: Arc<dyn RefStore>,
    read_raw_calls: AtomicUsize,
    for_each_ref_calls: AtomicUsize,
    prepare_calls: AtomicUsize,
}

impl CountingRefStore {
    /// Wrap `inner` and start counters at zero.
    #[must_use]
    pub fn new(inner: Arc<dyn RefStore>) -> Self {
        Self {
            inner,
            read_raw_calls: AtomicUsize::new(0),
            for_each_ref_calls: AtomicUsize::new(0),
            prepare_calls: AtomicUsize::new(0),
        }
    }

    /// Number of [`RefStore::read_raw`] calls since creation.
    #[must_use]
    pub fn read_raw_calls(&self) -> usize {
        self.read_raw_calls.load(Ordering::Relaxed)
    }

    /// Number of [`RefStore::for_each_ref`] calls since creation.
    #[must_use]
    pub fn for_each_ref_calls(&self) -> usize {
        self.for_each_ref_calls.load(Ordering::Relaxed)
    }

    /// Number of [`RefStore::prepare`] calls since creation.
    #[must_use]
    pub fn prepare_calls(&self) -> usize {
        self.prepare_calls.load(Ordering::Relaxed)
    }
}

struct CountingPrepared<'a> {
    inner: Box<dyn PreparedRefTransaction + Send + 'a>,
}

impl PreparedRefTransaction for CountingPrepared<'_> {
    fn commit(self: Box<Self>) -> StoreResult<()> {
        self.inner.commit()
    }

    fn abort(self: Box<Self>) -> StoreResult<()> {
        self.inner.abort()
    }
}

impl RefStore for CountingRefStore {
    fn format(&self) -> RefStorageFormat {
        self.inner.format()
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        self.read_raw_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.read_raw(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.for_each_ref_calls.fetch_add(1, Ordering::Relaxed);
        self.inner.for_each_ref(prefix, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        self.prepare_calls.fetch_add(1, Ordering::Relaxed);
        let inner = self.inner.prepare(txn)?;
        Ok(Box::new(CountingPrepared { inner }))
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
}
