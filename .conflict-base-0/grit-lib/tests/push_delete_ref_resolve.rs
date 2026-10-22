//! Unit tests for [`grit_lib::remote::transfer::resolve_push_delete_ref`].

use grit_lib::remote::transfer::resolve_push_delete_ref;
use std::collections::HashSet;

#[test]
fn prefers_remote_branch_when_local_branch_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut remote = HashSet::new();
    remote.insert("refs/heads/old-topic".to_owned());
    assert_eq!(
        resolve_push_delete_ref(&git_dir, &remote, "old-topic"),
        "refs/heads/old-topic"
    );
}

#[test]
fn uses_remote_tag_when_no_branch_on_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let mut remote = HashSet::new();
    remote.insert("refs/tags/v1".to_owned());
    assert_eq!(
        resolve_push_delete_ref(&git_dir, &remote, "v1"),
        "refs/tags/v1"
    );
}
