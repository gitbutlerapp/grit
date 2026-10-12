//! Additional linked worktree scenarios for admin layout and branch rules.

use grit_lib::repo::Repository;
use grit_lib::worktree::{
    add_linked_worktree, ensure_branch_not_checked_out_elsewhere,
    find_worktree_with_branch_checked_out, list_worktrees, remove_linked_worktree,
    AddLinkedWorktreeOptions,
};
use grit_test_support::git;
use std::fs;
use std::path::Path;

fn git_cmd(root: &Path, args: &[&str]) -> String {
    git(root, args)
}

fn init_repo(root: &Path) {
    git_cmd(root, &["init", "-q", "-b", "main", "."]);
    git_cmd(root, &["config", "user.email", "t@example.com"]);
    git_cmd(root, &["config", "user.name", "Test"]);
}

#[test]
fn add_detach_linked_worktree_and_list_locked_state() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let linked = tmp.path().join("detached");
    add_linked_worktree(
        &Repository::discover(Some(&main)).expect("discover"),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: None,
            new_branch: false,
            detach: true,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("detach add");

    let entry = list_worktrees(&Repository::discover(Some(&main)).expect("discover"))
        .expect("list")
        .into_iter()
        .find(|e| e.path == linked.canonicalize().unwrap_or(linked.clone()))
        .expect("listed");
    assert!(entry.head.is_detached());

    let admin = entry.admin_dir;
    fs::write(admin.join("locked"), "reason\n").expect("lock");
    let listed =
        list_worktrees(&Repository::discover(Some(&main)).expect("discover")).expect("list");
    let locked = listed.iter().find(|e| e.path == entry.path).expect("find");
    assert!(locked.is_locked);
    assert_eq!(locked.lock_reason.as_deref(), Some("reason"));

    remove_linked_worktree(
        &Repository::discover(Some(&main)).expect("discover"),
        &linked,
        true,
    )
    .expect("force remove locked");
}

#[test]
fn add_worktree_on_existing_branch() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);
    git_cmd(&main, &["branch", "feature"]);

    let linked = tmp.path().join("feature-wt");
    add_linked_worktree(
        &Repository::discover(Some(&main)).expect("discover"),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some("feature".into()),
            new_branch: false,
            detach: false,
            start_point: None,
            force: false,
        },
    )
    .expect("existing branch checkout");
    assert!(linked.join("f").is_file());
}

#[test]
fn remove_dirty_worktree_requires_force() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let linked = tmp.path().join("dirty");
    add_linked_worktree(
        &Repository::discover(Some(&main)).expect("discover"),
        &AddLinkedWorktreeOptions {
            path: linked.clone(),
            branch: Some("dirty".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("add");
    fs::write(linked.join("f"), "dirty change\n").unwrap();
    let repo = Repository::discover(Some(&main)).expect("discover");
    assert!(remove_linked_worktree(&repo, &linked, false).is_err());
    remove_linked_worktree(&repo, &linked, true).expect("force remove");
}

#[test]
fn second_worktree_same_branch_is_rejected() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let main = tmp.path().join("main");
    fs::create_dir_all(&main).expect("mkdir");
    init_repo(&main);
    fs::write(main.join("f"), "1\n").unwrap();
    git_cmd(&main, &["add", "f"]);
    git_cmd(&main, &["commit", "-qm", "init"]);

    let first = tmp.path().join("first");
    add_linked_worktree(
        &Repository::discover(Some(&main)).expect("discover"),
        &AddLinkedWorktreeOptions {
            path: first.clone(),
            branch: Some("shared".into()),
            new_branch: true,
            detach: false,
            start_point: Some("HEAD".into()),
            force: false,
        },
    )
    .expect("first");

    let second = tmp.path().join("second");
    assert!(add_linked_worktree(
        &Repository::discover(Some(&main)).expect("discover"),
        &AddLinkedWorktreeOptions {
            path: second,
            branch: Some("shared".into()),
            new_branch: false,
            detach: false,
            start_point: None,
            force: false,
        },
    )
    .is_err());
    let repo = Repository::discover(Some(&main)).expect("discover");
    let found = find_worktree_with_branch_checked_out(&repo, "shared", None).expect("find");
    assert!(found.is_some());
    assert!(ensure_branch_not_checked_out_elsewhere(&repo, "shared").is_err());
}
