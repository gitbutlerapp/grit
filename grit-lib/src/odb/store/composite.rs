//! Ordered chain of [`ObjectStore`] backends: first hit wins on lookup; enumeration deduplicates ids.

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};

use super::{ObjectStore, ObjectStream};

/// Read-only store that consults an ordered list of backends.
///
/// # Errors
///
/// Reads ([`ObjectStore::read`], `read_into`, `read_info`, `open_stream`) skip past a backend
/// whose copy is damaged ([`Error::Zlib`] or [`Error::CorruptObject`]) and return a later
/// backend's good copy. When no backend returns the object, the first such corruption error is
/// returned instead of `Ok(None)`. Any other backend error is returned immediately.
#[derive(Debug)]
pub struct CompositeStore {
    stores: Vec<Arc<dyn ObjectStore>>,
    hash_algo: HashAlgo,
}

impl CompositeStore {
    /// Build a composite from `stores` in lookup order (first match wins).
    ///
    /// `hash_algo` is used when every child is empty; otherwise the first store's algorithm applies.
    #[must_use]
    pub fn new(stores: Vec<Arc<dyn ObjectStore>>, hash_algo: HashAlgo) -> Self {
        Self { stores, hash_algo }
    }

    /// Backends in lookup order (read-only).
    #[must_use]
    pub fn stores(&self) -> &[Arc<dyn ObjectStore>] {
        &self.stores
    }

    /// Ask each child in order and return the first hit.
    ///
    /// A child that reports a damaged copy ([`Error::Zlib`] or [`Error::CorruptObject`]) does not
    /// stop the lookup, since a later child may hold a good copy. If none does, the first such
    /// error is returned so a damaged object is never reported as missing. Any other error stops
    /// the lookup immediately.
    fn first_hit<'s, T>(
        &'s self,
        mut lookup: impl FnMut(&'s dyn ObjectStore) -> Result<Option<T>>,
    ) -> Result<Option<T>> {
        let mut damaged = None;
        for store in &self.stores {
            match lookup(store.as_ref()) {
                Ok(Some(found)) => return Ok(Some(found)),
                Ok(None) => {}
                Err(err @ (Error::Zlib(_) | Error::CorruptObject(_))) => {
                    damaged.get_or_insert(err);
                }
                Err(err) => return Err(err),
            }
        }
        damaged.map_or(Ok(None), Err)
    }
}

impl ObjectStore for CompositeStore {
    fn hash_algo(&self) -> HashAlgo {
        self.stores
            .first()
            .map(|store| store.hash_algo())
            .unwrap_or(self.hash_algo)
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        self.first_hit(|store| store.read(oid))
    }

    fn read_into(&self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
        self.first_hit(|store| store.read_into(oid, buf))
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        self.first_hit(|store| store.read_info(oid))
    }

    fn contains(&self, oid: &ObjectId) -> Result<bool> {
        for store in &self.stores {
            if store.contains(oid)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn open_stream(&self, oid: &ObjectId) -> Result<Option<ObjectStream<'_>>> {
        self.first_hit(|store| store.open_stream(oid))
    }

    fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        let mut seen = HashSet::new();
        for store in &self.stores {
            let stop = super::for_each_propagate_break(store.as_ref(), &mut |oid| {
                if !seen.insert(*oid) {
                    return ControlFlow::Continue(());
                }
                f(oid)
            })?;
            if stop {
                break;
            }
        }
        Ok(())
    }

    fn lookup_prefix(&self, prefix: &str, limit: usize, out: &mut Vec<ObjectId>) -> Result<()> {
        let mut seen: HashSet<ObjectId> = out.iter().copied().collect();
        for store in &self.stores {
            if limit != 0 && out.len() >= limit {
                return Ok(());
            }
            let sub_limit = if limit == 0 {
                0
            } else {
                limit.saturating_sub(out.len())
            };
            let mut scratch = Vec::new();
            store.lookup_prefix(prefix, sub_limit, &mut scratch)?;
            for oid in scratch {
                if seen.insert(oid) {
                    out.push(oid);
                    if limit != 0 && out.len() >= limit {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn refresh(&self) -> Result<bool> {
        let mut changed = false;
        for store in &self.stores {
            if store.refresh()? {
                changed = true;
            }
        }
        Ok(changed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::odb::store::{MemoryStore, WritableObjectStore};
    use crate::odb::WriteOptions;

    #[test]
    fn for_each_stops_after_break_across_layers() {
        let store_a = MemoryStore::new(HashAlgo::Sha1);
        let store_b = MemoryStore::new(HashAlgo::Sha1);
        store_a
            .write(ObjectKind::Blob, b"a", WriteOptions::default())
            .unwrap();
        store_b
            .write(ObjectKind::Blob, b"b", WriteOptions::default())
            .unwrap();
        let composite = CompositeStore::new(
            vec![
                Arc::new(store_a) as Arc<dyn ObjectStore>,
                Arc::new(store_b) as Arc<dyn ObjectStore>,
            ],
            HashAlgo::Sha1,
        );
        let mut calls = 0usize;
        composite
            .for_each_object(&mut |_| {
                calls += 1;
                ControlFlow::Break(())
            })
            .unwrap();
        assert_eq!(calls, 1);
    }

    #[test]
    fn first_hit_read_and_deduped_for_each() {
        let store_a = MemoryStore::new(HashAlgo::Sha1);
        let store_b = MemoryStore::new(HashAlgo::Sha1);
        let oid_a = store_a
            .write(ObjectKind::Blob, b"only-a", WriteOptions::default())
            .unwrap();
        let oid_b = store_b
            .write(ObjectKind::Blob, b"only-b", WriteOptions::default())
            .unwrap();
        let a = Arc::new(store_a) as Arc<dyn ObjectStore>;
        let b = Arc::new(store_b) as Arc<dyn ObjectStore>;
        let composite = CompositeStore::new(vec![a, b], HashAlgo::Sha1);
        assert_eq!(
            composite.read(&oid_a).unwrap().expect("a").data,
            b"only-a".as_slice()
        );
        assert_eq!(
            composite.read(&oid_b).unwrap().expect("b").data,
            b"only-b".as_slice()
        );
        let mut ids = Vec::new();
        composite
            .for_each_object(&mut |oid| {
                ids.push(*oid);
                ControlFlow::Continue(())
            })
            .unwrap();
        ids.sort_by_key(|o| o.to_hex());
        let mut expected = vec![oid_a, oid_b];
        expected.sort_by_key(|o| o.to_hex());
        assert_eq!(ids, expected);
    }

    /// Holds a damaged copy of every object in `inner`: reads fail with `make_err`.
    #[derive(Debug)]
    struct Rotten {
        inner: MemoryStore,
        make_err: fn() -> Error,
    }

    impl ObjectStore for Rotten {
        fn hash_algo(&self) -> HashAlgo {
            self.inner.hash_algo()
        }

        fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
            match self.inner.read(oid)? {
                Some(_) => Err((self.make_err)()),
                None => Ok(None),
            }
        }

        fn read_into(&self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<Option<ObjectKind>> {
            buf.clear();
            buf.extend_from_slice(b"garbage from a half-inflated copy");
            self.read(oid).map(|obj| obj.map(|o| o.kind))
        }

        fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
            self.read(oid).map(|obj| obj.map(|_| unreachable!()))
        }

        fn for_each_object(&self, f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
            self.inner.for_each_object(f)
        }
    }

    fn rotten_with(data: &[u8], make_err: fn() -> Error) -> (Arc<dyn ObjectStore>, ObjectId) {
        let inner = MemoryStore::new(HashAlgo::Sha1);
        let oid = inner
            .write(ObjectKind::Blob, data, WriteOptions::default())
            .unwrap();
        (Arc::new(Rotten { inner, make_err }), oid)
    }

    fn corrupt() -> Error {
        Error::CorruptObject("checksum mismatch".to_owned())
    }

    fn zlib() -> Error {
        Error::Zlib("invalid stored block lengths".to_owned())
    }

    fn assert_every_read_fails(
        composite: &CompositeStore,
        oid: &ObjectId,
        is_expected: fn(&Error) -> bool,
    ) {
        let err = composite.read(oid).unwrap_err();
        assert!(is_expected(&err), "read: {err:?}");
        let err = composite.read_into(oid, &mut Vec::new()).unwrap_err();
        assert!(is_expected(&err), "read_into: {err:?}");
        let err = composite.read_info(oid).unwrap_err();
        assert!(is_expected(&err), "read_info: {err:?}");
        let Err(err) = composite.open_stream(oid) else {
            panic!("open_stream returned Ok");
        };
        assert!(is_expected(&err), "open_stream: {err:?}");
    }

    #[test]
    fn corrupt_copy_with_no_good_copy_is_reported_not_missing() {
        let (rotten, oid) = rotten_with(b"precious data", corrupt);
        let empty = Arc::new(MemoryStore::new(HashAlgo::Sha1)) as Arc<dyn ObjectStore>;
        let composite = CompositeStore::new(vec![rotten, empty], HashAlgo::Sha1);
        assert_every_read_fails(&composite, &oid, |e| matches!(e, Error::CorruptObject(_)));
    }

    #[test]
    fn zlib_failure_with_no_good_copy_is_reported_not_missing() {
        let empty = Arc::new(MemoryStore::new(HashAlgo::Sha1)) as Arc<dyn ObjectStore>;
        let (rotten, oid) = rotten_with(b"precious data", zlib);
        let composite = CompositeStore::new(vec![empty, rotten], HashAlgo::Sha1);
        assert_every_read_fails(&composite, &oid, |e| matches!(e, Error::Zlib(_)));
    }

    #[test]
    fn first_corruption_error_wins_when_every_copy_is_damaged() {
        let (zlib_store, oid) = rotten_with(b"precious data", zlib);
        let (corrupt_store, _) = rotten_with(b"precious data", corrupt);
        let composite = CompositeStore::new(vec![zlib_store, corrupt_store], HashAlgo::Sha1);
        assert_every_read_fails(&composite, &oid, |e| matches!(e, Error::Zlib(_)));
    }

    #[test]
    fn good_copy_in_later_store_beats_corrupt_copy() {
        for make_err in [corrupt as fn() -> Error, zlib] {
            let (rotten, oid) = rotten_with(b"precious data", make_err);
            let good = MemoryStore::new(HashAlgo::Sha1);
            good.write(ObjectKind::Blob, b"precious data", WriteOptions::default())
                .unwrap();
            let composite = CompositeStore::new(
                vec![rotten, Arc::new(good) as Arc<dyn ObjectStore>],
                HashAlgo::Sha1,
            );

            assert_eq!(
                composite.read(&oid).unwrap().expect("good copy").data,
                b"precious data"
            );
            let mut buf = Vec::new();
            assert_eq!(
                composite.read_into(&oid, &mut buf).unwrap(),
                Some(ObjectKind::Blob)
            );
            assert_eq!(buf, b"precious data");
            let info = composite.read_info(&oid).unwrap().expect("good copy");
            assert_eq!((info.kind, info.size), (ObjectKind::Blob, 13));
            let mut stream = composite.open_stream(&oid).unwrap().expect("good copy");
            let mut streamed = Vec::new();
            std::io::Read::read_to_end(&mut stream.reader, &mut streamed).unwrap();
            assert_eq!(streamed, b"precious data");
        }
    }

    #[test]
    fn other_errors_still_stop_the_lookup() {
        let (rotten, oid) = rotten_with(b"precious data", || {
            Error::Io(std::io::Error::other("backend down"))
        });
        let good = MemoryStore::new(HashAlgo::Sha1);
        good.write(ObjectKind::Blob, b"precious data", WriteOptions::default())
            .unwrap();
        let composite = CompositeStore::new(
            vec![rotten, Arc::new(good) as Arc<dyn ObjectStore>],
            HashAlgo::Sha1,
        );
        assert!(matches!(composite.read(&oid), Err(Error::Io(_))));
    }

    #[test]
    fn contains_delegates_without_reading_payload() {
        let store_a = MemoryStore::new(HashAlgo::Sha1);
        let oid = store_a
            .write(ObjectKind::Blob, b"x", WriteOptions::default())
            .unwrap();
        let composite = CompositeStore::new(
            vec![Arc::new(store_a) as Arc<dyn ObjectStore>],
            HashAlgo::Sha1,
        );
        assert!(composite.contains(&oid).unwrap());
        assert!(!composite
            .contains(&ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").unwrap())
            .unwrap());
    }
}
