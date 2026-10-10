//! [`FilesSource`] conformance via the shared [`grit_test_support::odb_conformance`] suite.

use flate2::Compression;
use grit_lib::diagnostics::NullDiagnostics;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{FilesSource, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_lib::pack_store::PackStore;
use grit_test_support::git_supports_sha256;
use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};
use std::sync::Arc;

fn leak_temp_objects(algo: HashAlgo) -> (FilesSource, std::path::PathBuf) {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let objects_dir = dir.path().join("objects");
    std::fs::create_dir_all(&objects_dir).expect("objects dir");
    let pack = Arc::new(PackStore::new(objects_dir.clone()));
    let loose = grit_lib::odb::store::LooseStore::new(objects_dir, algo, Compression::default());
    let files =
        FilesSource::open(pack, loose, false, false, Arc::new(NullDiagnostics)).expect("open");
    (files, dir.path().to_path_buf())
}

fn seed_files(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> FilesSource {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let (store, _root) = leak_temp_objects(algo);
        for (kind, data) in specs {
            WritableObjectStore::write(&store, *kind, data, WriteOptions::default())
                .expect("seed write");
        }
        store
    }
}

fn make_files(algo: HashAlgo) -> impl Fn() -> FilesSource {
    move || leak_temp_objects(algo).0
}

#[test]
fn files_source_sha1_read_suite() {
    run_read_suite(seed_files(HashAlgo::Sha1));
}

#[test]
fn files_source_sha1_write_suite() {
    run_write_suite(make_files(HashAlgo::Sha1));
}

#[test]
fn files_source_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_files(HashAlgo::Sha256));
}

#[test]
fn files_source_sha256_write_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_write_suite(make_files(HashAlgo::Sha256));
}
