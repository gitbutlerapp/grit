//! Integration tests for [`grit_lib::porcelain::replay::replay_commit`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayOutcome, ReplayRequest};
use grit_lib::refs::resolve_ref;
use grit_lib::repo::Repository;
use grit_test_support::git;

const IDENT: &str = "Test User <t@example.com> 1700000000 +0000";

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "gc.auto", "0"]);
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(&root.join(".git"), Some(root)).expect("open")
}

fn commit_file(root: &Path, path: &str, contents: &str, msg: &str) -> ObjectId {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(&full, contents).expect("write");
    git(root, &["add", path]);
    git(root, &["commit", "-qm", msg]);
    oid_from_rev(root, "HEAD")
}

fn oid_from_rev(root: &Path, rev: &str) -> ObjectId {
    let hex = git(root, &["rev-parse", rev]).trim().to_owned();
    ObjectId::from_hex(&hex).expect("oid")
}

fn git_fsck_strict(root: &Path) {
    let out = Command::new("git")
        .current_dir(root)
        .args(["fsck", "--strict"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git fsck");
    assert!(
        out.status.success(),
        "git fsck --strict failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

fn git_cat_file_commit(root: &Path, oid: &ObjectId) -> String {
    git(root, &["cat-file", "-p", &oid.to_hex()])
}

fn snapshot_tree(root: &Path) -> Vec<u8> {
    let mut out = Vec::new();
    for rel in ["index", "HEAD", "refs/heads/main"] {
        let p = root.join(".git").join(rel);
        if p.exists() {
            out.extend_from_slice(rel.as_bytes());
            out.push(0);
            out.extend(fs::read(&p).unwrap_or_default());
        }
    }
    fn walk(dir: &Path, prefix: &Path, out: &mut Vec<u8>) {
        if !dir.is_dir() {
            return;
        }
        for entry in fs::read_dir(dir).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            let rel = prefix.join(path.file_name().unwrap());
            if path.is_dir() {
                walk(&path, &rel, out);
            } else {
                out.extend(rel.to_string_lossy().as_bytes());
                out.push(0);
                out.extend(fs::read(&path).expect("read worktree file"));
            }
        }
    }
    walk(root, Path::new(""), &mut out);
    out
}

fn replay_pick(repo: &Repository, commit: ObjectId) -> grit_lib::error::Result<ReplayOutcome> {
    replay_commit(
        repo,
        &ReplayRequest {
            commit,
            direction: ReplayDirection::Pick,
            committer: IDENT.to_owned(),
            message_override: None,
        },
    )
}

fn replay_revert(repo: &Repository, commit: ObjectId) -> grit_lib::error::Result<ReplayOutcome> {
    replay_commit(
        repo,
        &ReplayRequest {
            commit,
            direction: ReplayDirection::Revert,
            committer: IDENT.to_owned(),
            message_override: None,
        },
    )
}

#[test]
fn replay_pick_clean_commit_fsck_and_reflog() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "v1\n", "initial");
    let base = oid_from_rev(root.path(), "HEAD");
    let picked = {
        commit_file(root.path(), "a.txt", "v2\n", "change");
        oid_from_rev(root.path(), "HEAD")
    };
    git(root.path(), &["reset", "--hard", &base.to_hex()]);

    let repo = open_repo(root.path());
    let outcome = replay_pick(&repo, picked).expect("pick");
    let ReplayOutcome::Committed { oid, .. } = outcome else {
        panic!("expected committed pick, got {outcome:?}");
    };

    git_fsck_strict(root.path());
    assert_eq!(resolve_ref(&repo.git_dir, "HEAD").expect("head"), oid);
    let body = git_cat_file_commit(root.path(), &oid);
    assert!(body.contains(&format!("parent {base}")));
    assert!(body.contains("author Test"));
    assert!(body.contains("change"));

    let reflog = git(root.path(), &["reflog", "show", "-1", "main"]);
    assert!(reflog.contains("cherry-pick: change"), "{reflog}");
}

#[test]
fn replay_pick_conflict_leaves_repo_unchanged() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "base\n", "initial");
    let side = {
        commit_file(root.path(), "a.txt", "side\n", "side");
        oid_from_rev(root.path(), "HEAD")
    };
    git(root.path(), &["reset", "--hard", "HEAD~1"]);
    commit_file(root.path(), "a.txt", "other\n", "other");

    let repo = open_repo(root.path());
    let before = snapshot_tree(root.path());
    let outcome = replay_pick(&repo, side).expect("pick");
    assert!(matches!(outcome, ReplayOutcome::Conflicts { .. }));
    assert_eq!(before, snapshot_tree(root.path()));
}

#[test]
fn replay_pick_empty() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "same\n", "initial");
    let empty = {
        git(root.path(), &["commit", "--allow-empty", "-qm", "empty"]);
        oid_from_rev(root.path(), "HEAD")
    };
    git(root.path(), &["reset", "--hard", "HEAD~1"]);

    let repo = open_repo(root.path());
    let outcome = replay_pick(&repo, empty).expect("pick");
    assert_eq!(outcome, ReplayOutcome::Empty);
}

#[test]
fn replay_pick_already_applied() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "v1\n", "initial");
    let picked = {
        commit_file(root.path(), "b.txt", "v2\n", "feature");
        oid_from_rev(root.path(), "HEAD")
    };
    git(root.path(), &["reset", "--hard", "HEAD~1"]);
    commit_file(root.path(), "b.txt", "v2\n", "same change manually");

    let repo = open_repo(root.path());
    let outcome = replay_pick(&repo, picked).expect("pick");
    assert_eq!(outcome, ReplayOutcome::AlreadyApplied);
}

#[test]
fn replay_pick_rejects_merge_commit() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "1\n", "initial");
    git(root.path(), &["checkout", "-qb", "side"]);
    commit_file(root.path(), "b.txt", "2\n", "side");
    git(root.path(), &["checkout", "main"]);
    commit_file(root.path(), "a.txt", "3\n", "main");
    git(root.path(), &["merge", "--no-edit", "side"]);
    let merge = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["reset", "--hard", "HEAD~1"]);

    let repo = open_repo(root.path());
    let err = replay_pick(&repo, merge).unwrap_err();
    assert!(
        matches!(err, Error::MergeCommit { .. }),
        "unexpected error: {err:?}"
    );
}

#[test]
fn replay_pick_root_commit() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "main.txt", "on main\n", "mainline");
    git(root.path(), &["checkout", "--orphan", "orphan"]);
    git(root.path(), &["rm", "-rf", "--quiet", "."]);
    fs::write(root.path().join("orphan.txt"), "orphan only\n").expect("write");
    git(root.path(), &["add", "orphan.txt"]);
    git(root.path(), &["commit", "-qm", "orphan root"]);
    let root_commit = oid_from_rev(root.path(), "HEAD");
    git(root.path(), &["checkout", "main"]);

    let repo = open_repo(root.path());
    let outcome = replay_pick(&repo, root_commit).expect("pick root");
    let ReplayOutcome::Committed { oid, .. } = outcome else {
        panic!("expected commit, got {outcome:?}");
    };
    git_fsck_strict(root.path());
    let body = git_cat_file_commit(root.path(), &oid);
    assert!(body.contains("author Test"));
    assert!(fs::read_to_string(root.path().join("orphan.txt")).is_ok());
}

#[test]
fn replay_revert_commit_message_and_author() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "v1\n", "initial");
    let target = {
        commit_file(root.path(), "a.txt", "v2\n", "feature title");
        oid_from_rev(root.path(), "HEAD")
    };
    commit_file(root.path(), "b.txt", "keep\n", "follow-up");

    let repo = open_repo(root.path());
    let outcome = replay_revert(&repo, target).expect("revert");
    let ReplayOutcome::Committed { oid, .. } = outcome else {
        panic!("expected revert commit");
    };

    git_fsck_strict(root.path());
    let body = git_cat_file_commit(root.path(), &oid);
    assert!(body.contains("Revert \"feature title\""));
    assert!(body.contains(&format!("This reverts commit {}", target.to_hex())));
    assert!(body.contains("author Test User"));
    assert!(body.contains("committer Test User"));

    let reflog = git(root.path(), &["reflog", "show", "-1", "main"]);
    assert!(reflog.contains("revert:"), "{reflog}");
}

#[test]
fn replay_revert_conflict_unchanged() {
    let root = tempfile::tempdir().expect("tempdir");
    init_repo(root.path());
    commit_file(root.path(), "a.txt", "base\n", "initial");
    let target = {
        commit_file(root.path(), "a.txt", "feature\n", "feature");
        oid_from_rev(root.path(), "HEAD")
    };
    git(root.path(), &["reset", "--hard", "HEAD~1"]);
    commit_file(root.path(), "a.txt", "manual\n", "manual");

    let repo = open_repo(root.path());
    let before = snapshot_tree(root.path());
    let outcome = replay_revert(&repo, target).expect("revert");
    assert!(matches!(outcome, ReplayOutcome::Conflicts { .. }));
    assert_eq!(before, snapshot_tree(root.path()));
}
