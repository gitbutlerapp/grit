//! Custom [`ObjectStore`] backends surface [`ObjectError::Backend`].

#[allow(clippy::unwrap_used)]

use grit_lib::objects::error::{ObjectError, ObjectResult};
use grit_lib::objects::odb::store::{MemoryStore, ObjectStore};
use grit_lib::objects::{HashAlgo, ObjectId};

#[derive(Debug)]
struct FailingBackend {
    inner: MemoryStore,
}

impl ObjectStore for FailingBackend {
    fn hash_algo(&self) -> HashAlgo {
        self.inner.hash_algo()
    }

    fn read(&self, oid: &ObjectId) -> ObjectResult<Option<grit_lib::objects::Object>> {
        let _ = oid;
        Err(ObjectError::Backend {
            source: Box::new(std::io::Error::new(
                std::io::ErrorKind::Other,
                "injected backend failure",
            )),
        })
    }

    fn read_info(&self, oid: &ObjectId) -> ObjectResult<Option<grit_lib::objects::ObjectInfo>> {
        self.inner.read_info(oid)
    }

    fn for_each_object(
        &self,
        f: &mut dyn FnMut(&ObjectId) -> std::ops::ControlFlow<()>,
    ) -> ObjectResult<()> {
        self.inner.for_each_object(f)
    }
}

#[test]
fn odb_backend_error_propagates() {
    let backend = FailingBackend {
        inner: MemoryStore::new(HashAlgo::Sha1),
    };
    let oid = ObjectId::from_hex("deadbeefdeadbeefdeadbeefdeadbeefdeadbeef").expect("oid");
    let err = backend.read(&oid).expect_err("backend read fails");
    match err {
        ObjectError::Backend { .. } => {}
        other => panic!("expected backend object error, got {other:?}"),
    }
}
