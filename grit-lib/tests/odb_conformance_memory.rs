//! [`MemoryStore`] conformance via the shared [`grit_test_support::odb_conformance`] suite.

use grit_lib::objects::{HashAlgo, ObjectKind};
use grit_lib::odb::store::{MemoryStore, WritableObjectStore};
use grit_lib::odb::WriteOptions;
use grit_test_support::git_supports_sha256;
use grit_test_support::odb_conformance::broken_store::WrongReadInfo;
use grit_test_support::odb_conformance::{run_read_suite, run_write_suite};

fn seed_memory(algo: HashAlgo) -> impl Fn(&[(ObjectKind, Vec<u8>)]) -> MemoryStore {
    move |specs: &[(ObjectKind, Vec<u8>)]| {
        let store = MemoryStore::new(algo);
        for (kind, data) in specs {
            store
                .write(*kind, data, WriteOptions::default())
                .expect("seed write");
        }
        store
    }
}

fn make_memory(algo: HashAlgo) -> impl Fn() -> MemoryStore {
    move || MemoryStore::new(algo)
}

#[test]
fn memory_store_sha1_read_suite() {
    run_read_suite(seed_memory(HashAlgo::Sha1));
}

#[test]
fn memory_store_sha1_write_suite() {
    run_write_suite(make_memory(HashAlgo::Sha1));
}

#[test]
fn memory_store_sha256_read_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_read_suite(seed_memory(HashAlgo::Sha256));
}

#[test]
fn memory_store_sha256_write_suite() {
    if !git_supports_sha256() {
        return;
    }
    run_write_suite(make_memory(HashAlgo::Sha256));
}

#[test]
#[should_panic(expected = "read_info_matches_read: kind")]
fn broken_read_info_fails_conformance_case() {
    let seed = seed_memory(HashAlgo::Sha1);
    grit_test_support::odb_conformance::read_info_matches_read(&|specs| {
        WrongReadInfo::new(seed(specs))
    });
}
