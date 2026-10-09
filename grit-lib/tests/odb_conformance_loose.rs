//! [`LooseStore`] conformance via the shared [`grit_test_support::odb_conformance`] suite.

use flate2::Compression;
use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{LooseStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_test_support::git_supports_sha256;
use grit_test_support::odb_conformance::broken_store::WrongReadInfo;
use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};

fn leak_temp_objects(algo: HashAlgo) -> LooseStore {
    let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
    let objects_dir = dir.path().join("objects");
    std::fs::create_dir_all(&objects_dir).expect("objects dir");
    LooseStore::new(objects_dir, algo, Compression::default())
}

fn seed_loose(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> LooseStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let store = leak_temp_objects(algo);
        for (kind, data) in specs {
            WritableObjectStore::write(&store, *kind, data, WriteOptions::default())
                .expect("seed write");
        }
        store
    }
}

fn make_loose(algo: HashAlgo) -> impl Fn() -> LooseStore {
    move || leak_temp_objects(algo)
}

#[test]
fn loose_store_sha1_read_suite() {
    run_read_suite(seed_loose(HashAlgo::Sha1));
}

#[test]
fn loose_store_sha1_write_suite() {
    run_write_suite(make_loose(HashAlgo::Sha1));
}

#[test]
fn loose_store_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_loose(HashAlgo::Sha256));
}

#[test]
fn loose_store_sha256_write_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_write_suite(make_loose(HashAlgo::Sha256));
}

#[test]
#[should_panic(expected = "read_info_matches_read: kind")]
fn broken_read_info_fails_conformance_case() {
    let seed = seed_loose(HashAlgo::Sha1);
    grit_test_support::odb_conformance::read_info_matches_read(&|specs| {
        WrongReadInfo::new(seed(specs))
    });
}
