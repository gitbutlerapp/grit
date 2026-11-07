//! [`FilesSource`] primary store: full [`grit_test_support::repository_conformance`] suite.

use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::repository_conformance::{
    export_objects_to_files_repo, run_repository_suite, BackendFixture,
};
use std::path::Path;

struct FilesBackend;

impl BackendFixture for FilesBackend {
    fn name(&self) -> &str {
        "files"
    }

    fn init(&self, dir: &Path, opts: &InitOptions) -> Repository {
        let (repo, _) = Repository::init(dir, opts).expect("init");
        repo
    }

    fn export_to_files(&self, repo: &Repository, dest: &Path) {
        if repo.work_tree() == Some(dest) {
            return;
        }
        export_objects_to_files_repo(repo, dest);
    }
}

#[test]
fn files_backend_passes_repository_conformance_suite() {
    run_repository_suite(&FilesBackend);
}
