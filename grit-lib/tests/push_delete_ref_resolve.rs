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
fn prefers_local_tag_when_remote_tag_gone() {
    let tmp = tempfile::tempdir().unwrap();
    let git_dir = tmp.path().join(".git");
    std::fs::create_dir_all(&git_dir).unwrap();
    std::fs::write(git_dir.join("HEAD"), "ref: refs/heads/main\n").unwrap();
    let tag_ref = git_dir.join("refs").join("tags").join("v3.0.0");
    std::fs::create_dir_all(tag_ref.parent().unwrap()).unwrap();
    std::fs::write(tag_ref, "83352a61f6715ec3f0a297c6d6150725154fb833\n").unwrap();
    let remote = HashSet::new();
    assert_eq!(
        resolve_push_delete_ref(&git_dir, &remote, "v3.0.0"),
        "refs/tags/v3.0.0"
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
