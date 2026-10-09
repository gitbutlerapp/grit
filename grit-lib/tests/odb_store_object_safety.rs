//! Ensures [`ObjectStore`] and [`WritableObjectStore`] are object-safe for embedders.

use std::ops::ControlFlow;
use std::sync::Arc;

use grit_lib::error::Result;
use grit_lib::objects::{HashAlgo, Object, ObjectId, ObjectInfo, ObjectKind};
use grit_lib::odb::store::{ObjectStore, ObjectStream, WritableObjectStore};
use grit_lib::odb::WriteOptions;

struct ExternalStore {
    algo: HashAlgo,
}

impl std::fmt::Debug for ExternalStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExternalStore").finish()
    }
}

impl ObjectStore for ExternalStore {
    fn hash_algo(&self) -> HashAlgo {
        self.algo
    }

    fn read(&self, _oid: &ObjectId) -> Result<Option<Object>> {
        Ok(None)
    }

    fn read_info(&self, _oid: &ObjectId) -> Result<Option<ObjectInfo>> {
        Ok(None)
    }

    fn for_each_object(&self, _f: &mut dyn FnMut(&ObjectId) -> ControlFlow<()>) -> Result<()> {
        Ok(())
    }
}

impl WritableObjectStore for ExternalStore {
    fn write(&self, kind: ObjectKind, data: &[u8], _options: WriteOptions) -> Result<ObjectId> {
        Ok(grit_lib::hash::hash_object(self.algo, kind, data))
    }

    fn freshen(&self, _oid: &ObjectId) -> Result<bool> {
        Ok(false)
    }
}

#[test]
fn writable_object_store_object_safe_arc_dyn() {
    let store: Arc<dyn WritableObjectStore> = Arc::new(ExternalStore {
        algo: HashAlgo::Sha1,
    });
    assert_eq!(store.hash_algo(), HashAlgo::Sha1);
    let oid = store
        .write(ObjectKind::Blob, b"embed", WriteOptions::default())
        .unwrap();
    assert!(store.read(&oid).unwrap().is_none());
    let _stream: Option<ObjectStream<'_>> = store.open_stream(&oid).unwrap();
    let _ = Arc::clone(&store);
}
