//! `git init --shared` and repository open paths for `repo.rs` / shared-repository helpers.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use std::os::unix::fs::PermissionsExt;
use support::{commit_file, git};

#[test]
fn init_shared_group_adjusts_repository_file_modes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let wt = dir.path();
    git(wt, &["init", "--shared=group", "-b", "main"]);
    commit_file(wt, "a.txt", "a\n", "first");
    let git_dir = wt.join(".git");
    let config_mode = std::fs::metadata(git_dir.join("config"))
        .expect("config")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(config_mode & 0o660, 0o660, "group-shared config mode");
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
    assert!(grit.work_tree().is_some());
    grit.index_handle().load().expect("index load");
}
