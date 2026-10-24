//! Test-only store wrapper that fails on demand, standing in for a network- or
//! database-backed backend during an outage.

use std::io;
use std::ops::ControlFlow;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::{MemoryStore, ObjectStore, WritableObjectStore};
use crate::error::{Error, Result};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::odb::WriteOptions;

/// In-memory store whose existence probes and/or reads can be switched to fail
/// with [`Error::Io`] (`ConnectionReset`).
#[derive(Debug)]
pub(crate) struct FlakyStore {
    inner: MemoryStore,
    fail_contains: AtomicBool,
    fail_reads: AtomicBool,
}

impl FlakyStore {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            inner: MemoryStore::new(HashAlgo::Sha1),
            fail_contains: AtomicBool::new(false),
            fail_reads: AtomicBool::new(false),
        })
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

    fn check(flag: &AtomicBool) -> Result<()> {
        if flag.load(Ordering::Relaxed) {
            return Err(Error::Io(io::Error::new(
                io::ErrorKind::ConnectionReset,
                "backend down",
            )));
        }
        Ok(())
    }
}

/// Whether `err` is the failure [`FlakyStore`] injects.
pub(crate) fn is_injected(err: &Error) -> bool {
    matches!(err, Error::Io(e) if e.kind() == io::ErrorKind::ConnectionReset)
}

impl ObjectStore for FlakyStore {
    fn hash_algo(&self) -> HashAlgo {
        HashAlgo::Sha1
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        Self::check(&self.fail_reads)?;
        self.inner.read(oid)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        Self::check(&self.fail_reads)?;
        self.inner.read_info(oid)
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        Self::check(&self.fail_contains)?;
        self.inner.contains(oid)
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        Self::check(&self.fail_reads)?;
        self.inner.for_each_object(f)
    }
}

impl WritableObjectStore for FlakyStore {
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId> {
        Self::check(&self.fail_reads)?;
        self.inner.write(kind, data, options)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        Self::check(&self.fail_reads)?;
        self.inner.freshen(oid)
    }
}
