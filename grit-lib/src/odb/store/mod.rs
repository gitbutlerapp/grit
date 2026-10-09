//! Pluggable object storage backends and the [`ObjectStore`] trait surface.
//!
//! Backends implement read, metadata, streaming, iteration, and (optionally) write
//! operations without allocating error strings on cache misses: a missing object
//! is `Ok(None)`, not [`Error::ObjectNotFound`].
//!
//! Thread safety: [`ObjectStore`] requires `Send + Sync` so repositories can share
//! a store across threads; individual methods may take internal locks.

mod composite;
mod files;
pub mod loose;

pub use composite::CompositeStore;
pub use files::FilesSource;
pub use loose::LooseStore;
pub use midx::{MidxObjects, MidxObjectsStatus};
pub use packs::{PackFilter, PackedObjects};

use std::collections::HashMap;
use std::io::{self, Cursor, Read};
use std::ops::ControlFlow;
use std::sync::{Arc, RwLock};

use crate::error::{Error, Result};
use crate::hash;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use crate::odb::WriteOptions;

type MemoryObjectEntry = (ObjectKind, Arc<[u8]>);

/// Streaming handle for a single object's uncompressed payload.
///
/// The reader yields bytes after the Git object header (`"<kind> <size>\\0"`).
pub struct ObjectStream<'a> {
    /// Resolved object type.
    pub kind: ObjectKind,
    /// Uncompressed payload length in bytes.
    pub size: u64,
    /// Payload bytes; must not include the object header.
    pub reader: Box<dyn Read + Send + 'a>,
}

/// Read-only object storage: lookup, metadata, streaming, and enumeration.
///
/// Implementations must be safe to share across threads (`Send + Sync`). Missing
/// objects are reported as `Ok(None)`, not as errors.
pub trait ObjectStore: Send + Sync + std::fmt::Debug {
    /// Hash algorithm for objects written through this store.
    fn hash_algo(&self) -> HashAlgo;

    /// Load the full object, if present.
    ///
    /// # Errors
    ///
    /// Returns I/O or corruption errors from the backend; never [`Error::ObjectNotFound`].
    fn read(&self, oid: &ObjectId) -> Result<Option<Object>>;

    /// Object kind and size without loading the full payload.
    ///
    /// # Errors
    ///
    /// Same as [`Self::read`].
    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>>;

    /// Whether `oid` is stored in this backend (without reading the payload).
    ///
    /// # Errors
    ///
    /// Propagates backend failures other than "not found".
    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        Ok(self.read_info(oid)?.is_some())
    }

    /// Open a stream over the object's uncompressed payload.
    ///
    /// Default implementation buffers via [`Self::read`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::read`].
    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        let Some(obj) = self.read(oid)? else {
            return Ok(None);
        };
        let size = u64::try_from(obj.data.len()).map_err(|_| {
            Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
        })?;
        Ok(Some(ObjectStream {
            kind: obj.kind,
            size,
            reader: Box::new(Cursor::new(obj.data)),
        }))
    }

    /// Invoke `f` for every object id in this store until `f` returns [`ControlFlow::Break`].
    ///
    /// # Errors
    ///
    /// Propagates backend enumeration failures.
    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()>;

    /// Fill `out` with object ids whose hex form starts with `prefix` (case-insensitive).
    ///
    /// At most `limit` ids are appended (0 means no limit). When more than one object
    /// matches and `limit` is 1, callers treat the result as ambiguous if `out` would
    /// have held multiple matches.
    ///
    /// Default implementation scans via [`Self::for_each_object`]; indexed backends
    /// should override with a prefix search.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] when `prefix` is not valid hex or is longer
    /// than a full object id for this store's hash algorithm.
    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix = normalize_oid_prefix(prefix, self.hash_algo())?;
        let mut count = 0usize;
        self.for_each_object(&mut |oid| {
            if oid_hex_has_prefix(oid, prefix.as_str()) {
                out.push(*oid);
                count += 1;
                if limit != 0 && count >= limit {
                    return ControlFlow::Break(());
                }
            }
            ControlFlow::Continue(())
        })
    }

    /// Reload indexes or caches after on-disk changes (packs, alternates, MIDX).
    ///
    /// Returns `true` when the store's view of objects changed. Default is no-op.
    ///
    /// # Errors
    ///
    /// Propagates backend refresh failures.
    fn refresh(&self) -> Result<bool> {
        Ok(false)
    }
}

/// Object storage that supports inserting new objects.
pub trait WritableObjectStore: ObjectStore {
    /// Store canonical object bytes and return the object id.
    ///
    /// If the object already exists, returns the existing id without replacing
    /// stored bytes (Git loose-store idempotence).
    ///
    /// # Errors
    ///
    /// Returns I/O, hashing, or validation errors from the backend.
    fn write(&self, kind: ObjectKind, data: &[u8], options: WriteOptions) -> Result<ObjectId>;

    /// Store an object from a byte stream of known uncompressed size.
    ///
    /// Default implementation reads the stream into memory and calls [`Self::write`].
    ///
    /// # Errors
    ///
    /// Same as [`Self::write`], plus stream read failures.
    fn write_stream(
        &self,
        kind: ObjectKind,
        size: u64,
        reader: &mut dyn Read,
        options: WriteOptions,
    ) -> Result<ObjectId> {
        let size_usize = usize::try_from(size).map_err(|_| {
            Error::CorruptObject(format!("object size {size} exceeds address space"))
        })?;
        let mut buf = vec![0u8; size_usize];
        reader.read_exact(&mut buf).map_err(Error::Io)?;
        self.write(kind, &buf, options)
    }

    /// Mark `oid` as recently used so retention policies keep it (Git `freshen`).
    ///
    /// Returns `true` when the object was present and the backend recorded the touch.
    ///
    /// # Errors
    ///
    /// Propagates backend failures.
    fn freshen(&self, oid: &ObjectId) -> Result<bool>;
}

/// In-memory object map keyed by [`ObjectId`].
///
/// Intended to replace the legacy [`Odb`](super::Odb) overlay hash map in a later step.
#[derive(Debug)]
pub struct MemoryStore {
    algo: HashAlgo,
    objects: RwLock<HashMap<ObjectId, MemoryObjectEntry>>,
}

impl MemoryStore {
    /// Create an empty store using `algo` for [`WritableObjectStore::write`].
    #[must_use]
    pub fn new(algo: HashAlgo) -> Self {
        Self {
            algo,
            objects: RwLock::new(HashMap::new()),
        }
    }

    /// Insert `oid` without re-hashing (overlay and tests).
    pub(crate) fn insert_object(&self, oid: ObjectId, kind: ObjectKind, data: Arc<[u8]>) {
        if let Ok(mut guard) = self.objects.write() {
            guard.entry(oid).or_insert((kind, data));
        }
    }
}

impl ObjectStore for MemoryStore {
    fn hash_algo(&self) -> HashAlgo {
        self.algo
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        Ok(guard.get(oid).map(|(kind, data)| Object {
            kind: *kind,
            data: data.to_vec(),
        }))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        Ok(guard.get(oid).map(|(kind, data)| ObjectInfo {
            kind: *kind,
            size: u64::try_from(data.len()).unwrap_or(u64::MAX),
        }))
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        Ok(guard.contains_key(oid))
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let Some((kind, data)) = guard.get(oid) else {
            return Ok(None);
        };
        let size = u64::try_from(data.len()).map_err(|_| {
            Error::CorruptObject(format!("object size overflow for {}", oid.to_hex()))
        })?;
        Ok(Some(ObjectStream {
            kind: *kind,
            size,
            reader: Box::new(Cursor::new(Arc::clone(data))),
        }))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        for oid in guard.keys() {
            if f(oid).is_break() {
                break;
            }
        }
        Ok(())
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix = normalize_oid_prefix(prefix, self.hash_algo())?;
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let mut count = 0usize;
        for oid in guard.keys() {
            if oid_hex_has_prefix(oid, prefix.as_str()) {
                out.push(*oid);
                count += 1;
                if limit != 0 && count >= limit {
                    break;
                }
            }
        }
        Ok(())
    }
}

impl WritableObjectStore for MemoryStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        let oid = hash::hash_object(self.algo, kind, data);
        let mut guard = self
            .objects
            .write()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        guard
            .entry(oid)
            .or_insert_with(|| (kind, Arc::from(data.to_owned())));
        Ok(oid)
    }

    fn freshen(&self, oid: &ObjectId) -> Result<bool> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        Ok(guard.contains_key(oid))
    }
}

pub(super) fn normalize_oid_prefix(prefix: &str, algo: HashAlgo) -> Result<String> {
    if prefix.is_empty() {
        return Ok(String::new());
    }
    if !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(Error::InvalidObjectId(prefix.to_owned()));
    }
    let max = algo.hex_len();
    if prefix.len() > max {
        return Err(Error::InvalidObjectId(prefix.to_owned()));
    }
    Ok(prefix.to_ascii_lowercase())
}

pub(super) fn oid_hex_has_prefix(oid: &ObjectId, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    oid.to_hex().starts_with(prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn read_hit_and_miss() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        let data = b"hello";
        let oid = store
            .write(ObjectKind::Blob, data, WriteOptions::default())
            .unwrap();
        let obj = store.read(&oid).unwrap().expect("hit");
        assert_eq!(obj.kind, ObjectKind::Blob);
        assert_eq!(obj.data, data);
        assert!(store.read(&ObjectId::zero()).unwrap().is_none());
        assert!(store.read_info(&ObjectId::zero()).unwrap().is_none());
        assert!(!store.contains(&ObjectId::zero()).unwrap());
        assert!(store.contains(&oid).unwrap());
    }

    #[test]
    fn stream_four_mib_blob_in_sixty_four_kib_chunks() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        let payload = vec![0xABu8; 4 * 1024 * 1024];
        let oid = store
            .write(ObjectKind::Blob, &payload, WriteOptions::default())
            .unwrap();
        let mut stream = store.open_stream(&oid).unwrap().expect("stream");
        assert_eq!(stream.kind, ObjectKind::Blob);
        assert_eq!(stream.size, payload.len() as u64);
        let mut out = Vec::with_capacity(payload.len());
        let mut chunk = [0u8; 64 * 1024];
        loop {
            let n = stream.reader.read(&mut chunk).unwrap();
            if n == 0 {
                break;
            }
            out.extend_from_slice(&chunk[..n]);
        }
        assert_eq!(out, payload);
    }

    #[test]
    fn for_each_object_stops_on_break() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        for i in 0u8..5 {
            store
                .write(ObjectKind::Blob, &[i], WriteOptions::default())
                .unwrap();
        }
        let mut seen = 0usize;
        store
            .for_each_object(&mut |_| {
                seen += 1;
                ControlFlow::Break(())
            })
            .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn lookup_prefix_unique_ambiguous_and_none() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        let a = store
            .write(ObjectKind::Blob, b"a", WriteOptions::default())
            .unwrap();
        let b = store
            .write(ObjectKind::Blob, b"b", WriteOptions::default())
            .unwrap();
        let hex_a = a.to_hex();
        let unique_prefix = &hex_a[..12];
        let mut out = Vec::new();
        store.lookup_prefix(unique_prefix, 0, &mut out).unwrap();
        assert_eq!(out, vec![a]);

        let mut ambiguous = Vec::new();
        store.lookup_prefix("", 2, &mut ambiguous).unwrap();
        assert_eq!(ambiguous.len(), 2);
        assert!(ambiguous.contains(&a));
        assert!(ambiguous.contains(&b));

        let mut none = Vec::new();
        store
            .lookup_prefix("ffffffffffffffffffffffffffffffffffffffff", 0, &mut none)
            .unwrap();
        assert!(none.is_empty());
    }

    #[test]
    fn write_idempotent() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        let data = b"same";
        let oid1 = store
            .write(ObjectKind::Blob, data, WriteOptions::default())
            .unwrap();
        let oid2 = store
            .write(ObjectKind::Blob, data, WriteOptions::default())
            .unwrap();
        assert_eq!(oid1, oid2);
        let guard = store.objects.read().unwrap();
        assert_eq!(guard.len(), 1);
    }

    #[test]
    fn write_stream_default_and_freshen() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        let payload = b"streamed";
        let mut cursor = Cursor::new(*payload);
        let oid = store
            .write_stream(
                ObjectKind::Blob,
                payload.len() as u64,
                &mut cursor,
                WriteOptions::default(),
            )
            .unwrap();
        assert_eq!(
            store.read(&oid).unwrap().expect("written").data,
            payload.as_slice()
        );
        assert!(store.freshen(&oid).unwrap());
        assert!(!store.freshen(&ObjectId::zero()).unwrap());
    }

    #[test]
    fn refresh_default_false() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        assert!(!store.refresh().unwrap());
    }

    #[test]
    fn trait_defaults_via_blanket_test_store() {
        struct DefaultsOnly {
            inner: MemoryStore,
        }

        impl std::fmt::Debug for DefaultsOnly {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.inner.fmt(f)
            }
        }

        impl ObjectStore for DefaultsOnly {
            fn hash_algo(&self) -> HashAlgo {
                self.inner.hash_algo()
            }

            fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
                self.inner.read(oid)
            }

            fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
                self.inner.read_info(oid)
            }

            fn for_each_object(
                &self,
                f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
            ) -> Result<()> {
                self.inner.for_each_object(f)
            }
        }

        let store = DefaultsOnly {
            inner: MemoryStore::new(HashAlgo::Sha1),
        };
        let oid = WritableObjectStore::write(
            &store.inner,
            ObjectKind::Blob,
            b"x",
            WriteOptions::default(),
        )
        .unwrap();
        assert!(ObjectStore::contains(&store, &oid).unwrap());
        assert!(ObjectStore::open_stream(&store, &oid).unwrap().is_some());
        let mut prefixed = Vec::new();
        ObjectStore::lookup_prefix(&store, &oid.to_hex()[..6], 0, &mut prefixed).unwrap();
        assert_eq!(prefixed, vec![oid]);
    }
}
