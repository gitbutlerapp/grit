//! Ordered chain of [`ObjectStore`] backends: first hit wins on lookup; enumeration deduplicates ids.

use std::collections::HashSet;
use std::ops::ControlFlow;
use std::sync::Arc;

use crate::error::Result;
use crate::objects::{HashAlgo, Object, ObjectId, ObjectInfo};

use super::{ObjectStore, ObjectStream};

/// Read-only store that consults an ordered list of backends.
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
}

impl ObjectStore for CompositeStore {
    fn hash_algo(&self) -> HashAlgo {
        self.stores
            .first()
            .map(|store| store.hash_algo())
            .unwrap_or(self.hash_algo)
    }

    fn read(&self, oid: &ObjectId) -> Result<Option<Object>> {
        for store in &self.stores {
            if let Some(obj) = store.read(oid)? {
                return Ok(Some(obj));
            }
        }
        Ok(None)
    }

    fn read_info(&self, oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        for store in &self.stores {
            if let Some(info) = store.read_info(oid)? {
                return Ok(Some(info));
            }
        }
        Ok(None)
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
        for store in &self.stores {
            if let Some(stream) = store.open_stream(oid)? {
                return Ok(Some(stream));
            }
        }
        Ok(None)
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
            let mut scratch = Vec::new();
            store.lookup_prefix(prefix, 0, &mut scratch)?;
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
