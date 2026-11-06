//! Test-only store wrapper that fails on demand, standing in for a network- or
//! database-backed backend during an outage.

use std::io;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use super::{MemoryStore, ObjectStore, WritableObjectStore};
use crate::error::Error;
use crate::objects::error::{ObjectError, ObjectResult};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::odb::WriteOptions;

/// In-memory store whose existence probes and/or reads can be switched to fail
/// with [`Error::Io`] (`ConnectionReset`), and which counts full object reads.
#[derive(Debug)]
pub(crate) struct FlakyStore {
    inner: MemoryStore,
    fail_contains: AtomicBool,
    fail_reads: AtomicBool,
    reads: AtomicUsize,
}

impl FlakyStore {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(HashAlgo::Sha1),
            fail_contains: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
            reads: AtomicUsize::new(0),
        })
    }

    /// Full object reads ([`ObjectStore::read`] / [`ObjectStore::read_into`]) so far.
    pub(crate) fn reads(&self) -> usize {
        self.reads.load(Ordering::Relaxed)
    }

    /// Make `contains` fail (reads keep working, e.g. a separate metadata endpoint is down).
    pub(crate) fn fail_contains(&self, fail: bool) {
        self.fail_contains.store(fail, Ordering::Relaxed);
    }

    /// Make every call fail: `contains`, reads, enumeration and writes.
    pub(crate) fn fail_all(&self, fail: bool) {
        self.fail_contains(fail);
        self.fail_reads.store(fail, Ordering::Relaxed);
    }

    fn check(flag: &AtomicBool) -> ObjectResult<()> {
        if flag.load(Ordering::Relaxed) {
            return Err(ObjectError::from(Error::Io(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "backend down",
            ))));
        }
        Ok(())
    }
}

/// Whether `err` is the failure [`FlakyStore`] injects.
pub(crate) fn is_injected(err: &ObjectError) -> bool {
    match err {
        ObjectError::Backend { source } => source
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::ConnectionReset),
        ObjectError::Nested(inner) => is_injected_error(inner.as_ref()),
        _ => false,
    }
}

/// Like [`is_injected`], for umbrella errors at repository boundaries.
pub(crate) fn is_injected_error(err: &Error) -> bool {
    match err {
        Error::Object(inner) => is_injected(inner),
        Error::Io(e) => e.kind() == io::ErrorKind::ConnectionReset,
        _ => false,
    }
}

impl ObjectStore for FlakyStore {
    fn hash_algo(&self) -> HashAlgo {
        HashAlgo::Sha1
    }

    fn read(&self, oid: &ObjectId) -> ObjectResult<Option<Object>> {
        Self::check(&self.fail_reads)?;
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> ObjectResult<Option<ObjectInfo>> {
        Self::check(&self.fail_reads)?;
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> ObjectResult<bool> {
        Self::check(&self.fail_contains)?;
        self.inner.contains(oid)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> ObjectResult<()> {
        Self::check(&self.fail_reads)?;
        self.inner.for_each_object(f)
    }
}

impl WritableObjectStore for FlakyStore {
    fn write(
        &self,
        kind: ObjectKind,
        data: &[u8],
        options: WriteOptions,
    ) -> ObjectResult<ObjectId> {
        Self::check(&self.fail_reads)?;
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> ObjectResult<bool> {
        Self::check(&self.fail_reads)?;
        self.inner.freshen(oid)
    }
}
