//! Issue #1321: merge allows uncommitted edits on paths the merge does not touch.

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn grit_repo(tmp: &tempfile::TempDir) -> Repository {
    Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("open grit repo")
}

#[test]
fn merge_fast_forward_preserves_dirty_unrelated_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("a.txt"), "a\n").expect("write a");
    std::fs::write(tmp.path().join("b.txt"), "b\n").expect("write b");
    git(tmp.path(), &["add", "a.txt", "b.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    let main_oid: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    git(tmp.path(), &["switch", "-c", "side"]);
    std::fs::write(tmp.path().join("a.txt"), "a-side\n").expect("write a side");
    git(tmp.path(), &["add", "a.txt"]);
    git(tmp.path(), &["commit", "-m", "side"]);
    let side_oid: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("side");

    git(tmp.path(), &["switch", "main"]);
    std::fs::write(tmp.path().join("b.txt"), "b dirty\n").expect("dirty b");

    let git_merge = Command::new("git")
        .current_dir(tmp.path())
        .args(["merge", "side"])
        .status()
        .expect("git merge");
    assert!(
        git_merge.success(),
        "system git must fast-forward merge with unrelated dirty b.txt"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("b.txt")).expect("read b"),
        "b dirty\n"
    );
    assert_eq!(
        git(tmp.path(), &["rev-parse", "HEAD"]).trim(),
        side_oid.to_hex()
    );

    let main_hex = main_oid.to_hex();
    git(tmp.path(), &["reset", "--hard", main_hex.as_str()]);
    std::fs::write(tmp.path().join("b.txt"), "b dirty\n").expect("dirty b again");

    let repo = grit_repo(&tmp);
    let outcome =
        integrate_branch(&repo, "refs/heads/main", main_oid, side_oid, "side").expect("merge");
    assert!(matches!(
        outcome,
        IntegrateBranchOutcome::FastForward { oid } if oid == side_oid
    ));
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("b.txt")).expect("read b after grit"),
        "b dirty\n"
    );
    assert_eq!(
        git(tmp.path(), &["rev-parse", "HEAD"]).trim(),
        side_oid.to_hex()
    );
    git(tmp.path(), &["fsck", "--strict"]);
}

#[test]
fn merge_refuses_dirty_overlapping_path() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("a.txt"), "a\n").expect("write a");
    git(tmp.path(), &["add", "a.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    let main_oid: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    git(tmp.path(), &["switch", "-c", "side"]);
    std::fs::write(tmp.path().join("a.txt"), "a-side\n").expect("write a side");
    git(tmp.path(), &["add", "a.txt"]);
    git(tmp.path(), &["commit", "-m", "side"]);
    let side_oid: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("side");

    git(tmp.path(), &["switch", "main"]);
    std::fs::write(tmp.path().join("a.txt"), "a dirty\n").expect("dirty a");

    let repo = grit_repo(&tmp);
    let err = integrate_branch(&repo, "refs/heads/main", main_oid, side_oid, "side")
        .expect_err("merge must refuse overlapping dirty path");
    match err {
        Error::Message(msg) => {
            assert!(
                msg.contains("a.txt") || msg.contains("uncommitted changes"),
                "expected overwrite/uncommitted error, got: {msg}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
    assert_eq!(
        git(tmp.path(), &["rev-parse", "HEAD"]).trim(),
        main_oid.to_hex()
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("a.txt")).expect("dirty preserved"),
        "a dirty\n"
    );
}

#[test]
fn merge_fast_forward_allows_worktree_deleted_unrelated_tracked_file() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(tmp.path(), &["init", "-q", "-b", "main"]);
    git(tmp.path(), &["config", "user.email", "t@example.com"]);
    git(tmp.path(), &["config", "user.name", "Test"]);

    std::fs::write(tmp.path().join("a.txt"), "a\n").expect("write a");
    std::fs::write(tmp.path().join("b.txt"), "b\n").expect("write b");
    git(tmp.path(), &["add", "a.txt", "b.txt"]);
    git(tmp.path(), &["commit", "-m", "base"]);
    let main_oid: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("main");

    git(tmp.path(), &["switch", "-c", "side"]);
    std::fs::write(tmp.path().join("a.txt"), "a-side\n").expect("write a side");
    git(tmp.path(), &["add", "a.txt"]);
    git(tmp.path(), &["commit", "-m", "side"]);
    let side_oid: ObjectId = git(tmp.path(), &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("side");

    git(tmp.path(), &["switch", "main"]);
    std::fs::remove_file(tmp.path().join("b.txt")).expect("delete b from worktree");

    let repo = grit_repo(&tmp);
    integrate_branch(&repo, "refs/heads/main", main_oid, side_oid, "side").expect("merge");
    assert!(
        !tmp.path().join("b.txt").exists(),
        "deleted unrelated file stays absent after ff merge"
    );
    assert_eq!(
        git(tmp.path(), &["rev-parse", "HEAD"]).trim(),
        side_oid.to_hex()
    );
}
