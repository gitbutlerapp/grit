//! [`CompositeStore`] conformance via the shared read suite.

use flate2::Compression;
use grit_lib::diagnostics::NullDiagnostics;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{CompositeStore, FilesSource, ObjectStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_lib::pack_store::PackStore;
use grit_test_support::odb_conformance::run_read_suite;
use std::sync::Arc;

fn leak_files(algo: HashAlgo) -> Arc<FilesSource> {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let objects_dir = dir.path().join("objects");
    std::fs::create_dir_all(&objects_dir).expect("objects");
    let pack = Arc::new(PackStore::new(objects_dir.clone()));
    let loose = grit_lib::odb::store::LooseStore::new(objects_dir, algo, Compression::default());
    Arc::new(FilesSource::open(pack, loose, false, false, Arc::new(NullDiagnostics)).expect("open"))
}

fn seed_composite(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> CompositeStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let layer = leak_files(algo);
        for (kind, data) in specs {
            WritableObjectStore::write(layer.as_ref(), *kind, data, WriteOptions::default())
                .expect("seed");
        }
        let layer_dyn: Arc<dyn ObjectStore> = layer;
        CompositeStore::new(vec![layer_dyn], algo)
    }
}

#[test]
fn composite_store_sha1_read_suite() {
    run_read_suite(seed_composite(HashAlgo::Sha1));
}

#[test]
fn composite_store_two_layer_first_hit() {
    let algo = HashAlgo::Sha1;
    let first = leak_files(algo);
    let second = leak_files(algo);
    let oid_first = first
        .write(ObjectKind::Blob, b"first-layer", WriteOptions::default())
        .unwrap();
    let _oid_second = second
        .write(ObjectKind::Blob, b"second-layer", WriteOptions::default())
        .unwrap();
    let composite = CompositeStore::new(
        vec![
            first as Arc<dyn ObjectStore>,
            second as Arc<dyn ObjectStore>,
        ],
        algo,
    );
    assert_eq!(
        composite.read(&oid_first).unwrap().expect("hit").data,
        b"first-layer".as_slice()
    );
}
