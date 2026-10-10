//! Conformance suite for [`grit_lib::refs::store::FilesRefStore`].

use std::fs;
use std::sync::{Arc, Mutex};

use grit_lib::refs::store::{FilesRefStore, FilesRefStoreConfig, RefStore};
use grit_lib::refs::LogRefsConfig;

mod support;

fn files_store_factory() -> Arc<dyn Fn() -> Arc<dyn RefStore> + Send + Sync> {
    let counter = Arc::new(Mutex::new(0u64));
    Arc::new(move || {
        let n = {
            let mut c = counter.lock().expect("lock");
            *c += 1;
            *c
        };
        let base = std::env::temp_dir().join(format!("grit-files-refstore-{n}"));
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(base.join("refs/heads")).expect("refs");
        fs::create_dir_all(base.join("objects")).expect("objects");
        let git_dir = fs::canonicalize(&base).unwrap_or(base);
        let store = FilesRefStore::open(FilesRefStoreConfig {
            git_dir: git_dir.clone(),
            common_dir: git_dir,
            namespace_prefix: None,
            log_refs: LogRefsConfig::Normal,
        });
        Arc::new(store) as Arc<dyn RefStore>
    })
}

#[test]
fn files_refstore_passes_conformance_suite() {
    support::run_refstore_conformance(files_store_factory());
}
