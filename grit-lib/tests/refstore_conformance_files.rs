//! Conformance suite for [`grit_lib::refs::store::FilesRefStore`].

use std::fs;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use grit_lib::refs::store::{FilesRefStore, FilesRefStoreConfig, RefStore};
use grit_test_support::refstore_conformance::run_suite;

fn files_store() -> Arc<dyn RefStore> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let base = std::env::temp_dir().join(format!("grit-files-refstore-{}-{n}", std::process::id()));
    let _ = fs::remove_dir_all(&base);
    fs::create_dir_all(base.join("refs/heads")).expect("refs");
    fs::create_dir_all(base.join("objects")).expect("objects");
    let git_dir = fs::canonicalize(&base).unwrap_or(base);
    Arc::new(FilesRefStore::open(FilesRefStoreConfig {
        git_dir: git_dir.clone(),
        common_dir: git_dir,
        namespace_prefix: None,
    }))
}

#[test]
fn files_refstore_passes_conformance_suite() {
    run_suite(files_store);
}
