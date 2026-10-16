//! `format_porcelain_v2` vs system `git status --porcelain=v2`.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;

use support::{assert_porcelain_v2_matches_git, git, git_ok, HistoryRepo};

#[test]
fn t_porcelain_v2_tracked_file_deletion_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("file.txt"), "payload\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "add file"]);
    std::fs::remove_file(wt.join("file.txt")).expect("delete worktree file");

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_modified_file_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("file.txt"), "a\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "init"]);
    std::fs::write(wt.join("file.txt"), "b\n").expect("modify");

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_staged_add_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("keep.txt"), "x\n").expect("write");
    git(wt, &["add", "keep.txt"]);
    git(wt, &["commit", "-m", "base"]);
    std::fs::write(wt.join("new.txt"), "n\n").expect("write");
    git(wt, &["add", "new.txt"]);
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_staged_delete_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("f.txt"), "x\n").expect("write");
    git(wt, &["add", "f.txt"]);
    git(wt, &["commit", "-m", "base"]);
    git(wt, &["rm", "--cached", "f.txt"]);
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_staged_chmod_matches_git() {
    use std::os::unix::fs::PermissionsExt;

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("exec.sh"), "#!/bin/sh\n").expect("write");
    git(wt, &["add", "exec.sh"]);
    git(wt, &["commit", "-m", "base"]);
    std::fs::set_permissions(wt.join("exec.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    git(wt, &["add", "exec.sh"]);
    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_intent_to_add_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("u.txt"), "payload\n").expect("write");
    git(wt, &["add", "-N", "u.txt"]);

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_intent_to_add_sha256_matches_git() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let wt = dir.path();
    if !git_ok(
        wt,
        &["init", "--object-format=sha256", "--initial-branch=main"],
    ) {
        eprintln!("SKIP: git lacks --object-format=sha256");
        return;
    }
    git(wt, &["config", "user.email", "test@example.com"]);
    git(wt, &["config", "user.name", "Test"]);
    std::fs::write(wt.join("u.txt"), "payload\n").expect("write");
    git(wt, &["add", "-N", "u.txt"]);

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}

#[test]
fn t_porcelain_v2_unmerged_conflict_matches_git() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    std::fs::write(wt.join("f.txt"), "base\n").expect("write");
    git(wt, &["add", "f.txt"]);
    git(wt, &["commit", "-m", "base"]);
    git(wt, &["checkout", "-b", "side"]);
    std::fs::write(wt.join("f.txt"), "side\n").expect("write");
    git(wt, &["commit", "-am", "side"]);
    git(wt, &["checkout", "main"]);
    std::fs::write(wt.join("f.txt"), "main\n").expect("write");
    git(wt, &["commit", "-am", "main"]);
    assert!(
        !git_ok(wt, &["merge", "side"]),
        "merge should stop with a conflict"
    );

    let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
    assert_porcelain_v2_matches_git(wt, &grit);
}
