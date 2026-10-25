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
#[cfg(test)]
pub(crate) mod flaky;
pub mod loose;
mod midx;
mod packs;

pub use composite::CompositeStore;
pub use files::FilesSource;
pub use loose::LooseStore;
pub use midx::{MidxObjects, MidxObjectsStatus};
pub use packs::{PackFilter, PackedObjects};

use std::io::{self, Cursor, Read};
use std::ops::ControlFlow;
use std::sync::{Arc, RwLock};

use std::path::Path;

use crate::error::{Error, Result};
use crate::hash;
use crate::objects::{
    oid_hash_map, HashAlgo, Object, ObjectId, ObjectIdPrefix, ObjectInfo, ObjectKind, OidHashMap,
};
use crate::odb::WriteOptions;
use crate::pack;

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

    /// Load object payload into `buf`, reusing its capacity when possible.
    ///
    /// Default implementation delegates to [`Self::read`] and copies into `buf`.
    /// Native backends should inflate or resolve deltas directly into `buf`.
    ///
    /// # Errors
    ///
    /// Same as [`Self::read`].
    fn read_into(&self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
        let Some(obj) = self.read(oid)? else {
            return Ok(None);
        };
        buf.clear();
        buf.extend_from_slice(&obj.data);
        Ok(Some(obj.kind))
    }

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

    /// For each id in `oids`, whether it is stored in this backend; the result has
    /// one entry per input id, in input order.
    ///
    /// Callers that need to check many objects at once (for example write-tree
    /// confirming that every reusable cached subtree still exists) use this instead
    /// of one [`Self::contains`] call per id. The default implementation calls
    /// [`Self::contains`] for each id. A database- or network-backed store should
    /// override it to answer the whole batch in a single round trip.
    ///
    /// # Errors
    ///
    /// Propagates the first backend failure, like [`Self::contains`].
    fn contains_many(&self, oids: &[ObjectId]) -> Result<Vec<bool>> {
        oids.iter().map(|oid| self.contains(oid)).collect()
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
    /// The default implementation parses `prefix` once into an [`ObjectIdPrefix`] and
    /// compares it against the raw bytes of every id from [`Self::for_each_object`]: one
    /// allocation-free pass over the whole store per call. Abbreviating ids and resolving
    /// short ids call this once per id, so a store with many objects and a cheaper way to
    /// find ids by leading bytes (a sorted index, an ordered key-value range from
    /// [`ObjectIdPrefix::lowest`] to [`ObjectIdPrefix::highest`]) should override it.
    ///
    /// # Errors
    ///
    /// Returns [`Error::InvalidObjectId`] when `prefix` is not valid hex or is longer
    /// than a full object id for this store's hash algorithm.
    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let prefix = ObjectIdPrefix::from_hex(prefix, self.hash_algo())?;
        let mut count = 0usize;
        self.for_each_object(&mut |oid| {
            if prefix.matches(oid) {
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

/// True when `oid` is stored as loose or in a non-promisor pack under `objects_dir`.
pub(crate) fn local_object_materialized(objects_dir: &Path, oid: &ObjectId) -> bool {
    if local_object_materialized_without_rescan(objects_dir, oid) {
        return true;
    }
    if pack::reprepare_pack_directory_on_miss(objects_dir).ok() == Some(true) {
        return object_in_local_packs(objects_dir, oid);
    }
    false
}

/// Like [`local_object_materialized`], but trusts the cached pack listing instead of
/// rescanning `pack/` when the object is not found.
pub(crate) fn local_object_materialized_without_rescan(objects_dir: &Path, oid: &ObjectId) -> bool {
    object_in_local_packs(objects_dir, oid) || oid.loose_path_in(objects_dir).is_file()
}

fn object_in_local_packs(objects_dir: &Path, oid: &ObjectId) -> bool {
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return false;
    };
    for idx in &indexes {
        if idx.is_promisor {
            continue;
        }
        if idx.contains(oid).unwrap_or(false) {
            return true;
        }
    }
    false
}

/// True when `oid` is present as loose or in any local pack index (including promisor).
///
/// Does not consult alternates, trigger lazy fetch, or rescan `pack/` on miss beyond the
/// cached index listing (same as [`local_object_materialized_without_rescan`]).
pub(crate) fn local_object_stored_without_rescan(objects_dir: &Path, oid: &ObjectId) -> bool {
    let Ok(indexes) = pack::read_local_pack_indexes_cached(objects_dir) else {
        return oid.loose_path_in(objects_dir).is_file();
    };
    for idx in &indexes {
        if idx.contains(oid).unwrap_or(false) {
            return true;
        }
    }
    oid.loose_path_in(objects_dir).is_file()
}

/// Like [`local_object_stored_without_rescan`], but rescan `pack/` once on miss.
pub(crate) fn local_object_stored(objects_dir: &Path, oid: &ObjectId) -> bool {
    if local_object_stored_without_rescan(objects_dir, oid) {
        return true;
    }
    if pack::reprepare_pack_directory_on_miss(objects_dir).ok() == Some(true) {
        return local_object_stored_without_rescan(objects_dir, oid);
    }
    false
}

/// In-memory object map keyed by [`ObjectId`].
///
/// Intended to replace the legacy [`Odb`](super::Odb) overlay hash map in a later step.
#[derive(Debug)]
pub struct MemoryStore {
    algo: HashAlgo,
    objects: RwLock<OidHashMap<MemoryObjectEntry>>,
}

impl MemoryStore {
    /// Create an empty store using `algo` for [`WritableObjectStore::write`].
    #[must_use]
    pub fn new(algo: HashAlgo) -> Self {
        Self {
            algo,
            objects: RwLock::new(oid_hash_map()),
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

    fn contains_many(&self, oids: &[ObjectId]) -> Result<Vec<bool>> {
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        Ok(oids.iter().map(|oid| guard.contains_key(oid)).collect())
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
        let prefix = ObjectIdPrefix::from_hex(prefix, self.hash_algo())?;
        let guard = self
            .objects
            .read()
            .map_err(|e| Error::Io(io::Error::other(e.to_string())))?;
        let matches = guard.keys().filter(|oid| prefix.matches(oid)).copied();
        if limit == 0 {
            out.extend(matches);
        } else {
            out.extend(matches.take(limit));
        }
        Ok(())
    }
}

impl WritableObjectStore for MemoryStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        let oid = hash::hash_object(self.algo, kind, data)?;
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

/// Ask `probe` about the ids of `oids` indexed by `pending`; mark each hit in `present` and
/// drop it from `pending`, so the next source is only asked about what is still missing.
///
/// Does nothing when `pending` is empty. A short answer from `probe` counts as "absent".
pub(crate) fn resolve_pending(
    oids: &[ObjectId],
    present: &mut [bool],
    pending: &mut Vec<usize>,
    probe: impl FnOnce(&[ObjectId]) -> Result<Vec<bool>>,
) -> Result<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let batch: Vec<ObjectId> = pending.iter().map(|&i| oids[i]).collect();
    let mut found = probe(&batch)?.into_iter();
    pending.retain(|&i| {
        let hit = found.next().unwrap_or(false);
        present[i] |= hit;
        !hit
    });
    Ok(())
}

/// Run `store.for_each_object` and stop visiting further stores when `f` returns [`ControlFlow::Break`].
pub(crate) fn for_each_propagate_break(
    store: &dyn ObjectStore,
    f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>,
) -> Result<bool> {
    let mut stop = false;
    store.for_each_object(&mut |oid| {
        if stop {
            return ControlFlow::Break(());
        }
        match f(oid) {
            ControlFlow::Break(()) => {
                stop = true;
                ControlFlow::Break(())
            }
            ControlFlow::Continue(()) => ControlFlow::Continue(()),
        }
    })?;
    Ok(stop)
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
        assert_eq!(
            store.contains_many(&[oid, ObjectId::zero(), oid]).unwrap(),
            vec![true, false, true]
        );
        assert!(store.contains_many(&[]).unwrap().is_empty());
    }

    #[test]
    fn resolve_pending_marks_hits_and_keeps_misses() {
        let ids: Vec<ObjectId> = (1u8..=3)
            .map(|b| ObjectId::from_bytes(&[b; 20]).unwrap())
            .collect();
        let mut present = vec![false; 3];
        let mut pending = vec![0, 2];
        resolve_pending(&ids, &mut present, &mut pending, |batch| {
            assert_eq!(batch, [ids[0], ids[2]]);
            Ok(vec![false, true])
        })
        .unwrap();
        assert_eq!(present, [false, false, true]);
        assert_eq!(pending, [0]);

        resolve_pending(&ids, &mut present, &mut pending, |_| Ok(Vec::new())).unwrap();
        assert_eq!(pending, [0], "a short answer counts as absent");

        pending.clear();
        resolve_pending(&ids, &mut present, &mut pending, |_| {
            panic!("no probe when nothing is pending")
        })
        .unwrap();
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
    fn lookup_prefix_odd_even_empty_and_full_length() {
        let store = MemoryStore::new(HashAlgo::Sha1);
        let ids: Vec<ObjectId> = [
            "abcdef0123000000000000000000000000000000",
            "abcdef0124000000000000000000000000000000",
            "abd0000000000000000000000000000000000000",
        ]
        .iter()
        .map(|hex| ObjectId::from_hex(hex).unwrap())
        .collect();
        for id in &ids {
            store.insert_object(*id, ObjectKind::Blob, Arc::from(Vec::new()));
        }
        let lookup = |prefix: &str, limit: usize| {
            let mut out = Vec::new();
            store.lookup_prefix(prefix, limit, &mut out).unwrap();
            out.sort();
            out
        };
        assert_eq!(lookup("", 0), ids);
        assert_eq!(lookup("ab", 1).len(), 1);
        assert_eq!(lookup("abc", 0), ids[..2]);
        assert_eq!(lookup("abcdef012", 0), ids[..2]);
        assert_eq!(lookup("ABCDEF0124", 0), ids[1..2]);
        assert_eq!(lookup(&ids[2].to_hex(), 0), ids[2..]);
        assert!(lookup("abe", 0).is_empty());
        let mut out = Vec::new();
        assert!(matches!(
            store.lookup_prefix("xyz", 0, &mut out),
            Err(Error::InvalidObjectId(_))
        ));
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
        assert_eq!(
            ObjectStore::contains_many(&store, &[ObjectId::zero(), oid]).unwrap(),
            vec![false, true]
        );
        assert!(ObjectStore::open_stream(&store, &oid).unwrap().is_some());
        let mut prefixed = Vec::new();
        ObjectStore::lookup_prefix(&store, &oid.to_hex()[..6], 0, &mut prefixed).unwrap();
        assert_eq!(prefixed, vec![oid]);
    }
}
