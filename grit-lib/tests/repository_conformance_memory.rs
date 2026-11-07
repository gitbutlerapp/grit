//! [`MemoryStore`] primary with files refs: repository conformance suite.

use std::path::Path;
use std::sync::Arc;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::odb::store::MemoryStore;
use grit_lib::objects::HashAlgo;
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::repository_conformance::{
    export_objects_to_files_repo, run_repository_suite, BackendFixture,
};

struct MemoryBackend;

impl BackendFixture for MemoryBackend {
    fn name(&self) -> &str {
        "memory"
    }

    fn init(&self, dir: &Path, opts: &InitOptions) -> Repository {
        let store = Arc::new(MemoryStore::new(HashAlgo::Sha1));
        let merged = opts.clone().with_repository_options(
            opts.repository
                .clone()
                .with_object_store(store)
                .with_work_tree(dir),
        );
        let (repo, _) = Repository::init(dir, &merged).expect("init");
        repo
    }

    fn export_to_files(&self, repo: &Repository, dest: &Path) {
        export_objects_to_files_repo(repo, dest);
    }
}

#[test]
fn memory_primary_passes_repository_conformance_suite() {
    run_repository_suite(&MemoryBackend);
}
