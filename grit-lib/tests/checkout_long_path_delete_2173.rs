//! Switch and hard reset remove a tracked file whose repository path is longer than one
//! path component may be, though each of its components is short (issue #2173).

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
use grit_lib::progress::CancelToken;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn deep_path() -> String {
    format!("{}/deep.txt", vec!["d".repeat(40); 8].join("/"))
}

fn rev(root: &Path, rev: &str) -> ObjectId {
    ObjectId::from_hex(git(root, &["rev-parse", rev]).trim()).expect("oid")
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo")
}

fn switch_to(repo: &Repository, branch: &str, oid: ObjectId) {
    switch_repository(
        repo,
        &SwitchOptions {
            mode: SwitchMode::Existing {
                branch: branch.into(),
                oid,
            },
            dry_run: false,
            hooks: grit_lib::command_runner::hooks::PostOperationHooks::Skip,
        },
        &mut NullProgress,
        &CancelToken::never(),
    )
    .unwrap_or_else(|err| panic!("switch to {branch}: {err}"));
}

fn assert_consistent(root: &Path, head: ObjectId) {
    assert_eq!(rev(root, "HEAD"), head);
    let status = git(root, &["status", "--porcelain"]);
    assert!(status.trim().is_empty(), "worktree not clean:\n{status}");
    git(root, &["fsck", "--strict"]);
}

/// A repository on `main` holding the deep file, and a branch `gone` that deletes it.
fn repo_with_deleting_branch() -> (tempfile::TempDir, ObjectId, ObjectId) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    let deep = deep_path();
    assert!(deep.len() > 255);
    let abs = root.join(&deep);
    std::fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
    std::fs::write(&abs, "x\n").expect("write deep file");
    std::fs::write(root.join("keep.txt"), "keep\n").expect("write keep");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-q", "-c", "gone"]);
    git(root, &["rm", "-q", &deep]);
    git(root, &["commit", "-qm", "rm"]);
    git(root, &["switch", "-q", "main"]);
    let main = rev(root, "main");
    let gone = rev(root, "gone");
    (tmp, main, gone)
}

#[test]
fn switch_removes_and_recreates_a_deep_path() {
    let (tmp, main, gone) = repo_with_deleting_branch();
    let root = tmp.path();
    let deep = root.join(deep_path());
    let repo = open(root);

    switch_to(&repo, "gone", gone);
    assert!(!deep.exists(), "switch must delete the deep file");
    assert!(!root
        .join("dddddddddddddddddddddddddddddddddddddddd")
        .exists());
    assert_eq!(
        git(root, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/gone"
    );
    assert_consistent(root, gone);

    switch_to(&repo, "main", main);
    assert_eq!(std::fs::read(&deep).expect("deep file back"), b"x\n");
    assert_consistent(root, main);
}

#[test]
fn hard_reset_removes_a_deep_path() {
    let (tmp, _main, gone) = repo_with_deleting_branch();
    let root = tmp.path();
    let repo = open(root);
    let identity = "Test <t@example.com> 1700000000 +0000"
        .parse()
        .expect("identity");

    let outcome = reset_to_commit(
        &repo,
        &ResetOptions::new(gone, ResetMode::Hard, identity, "reset: moving to gone"),
        &mut NullProgress,
        &CancelToken::never(),
    )
    .expect("hard reset");

    assert_eq!(outcome.target, gone);
    assert!(
        !root.join(deep_path()).exists(),
        "reset must delete the deep file"
    );
    assert_eq!(
        git(root, &["symbolic-ref", "HEAD"]).trim(),
        "refs/heads/main"
    );
    assert_consistent(root, gone);
}
