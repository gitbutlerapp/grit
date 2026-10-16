//! Regression tests for commit change counts, allow-empty staging, and large-index untracked.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, stage_for_commit, StageOptions};
use grit_lib::porcelain::commit::{create_commit, CommitRequest};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

/// Same as [`grit_lib`]'s UNTR auto-cache entry threshold (see `untracked_cache` module).
const AUTO_UNTRACKED_CACHE_MIN_ENTRIES: usize = 4096;

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn ident() -> String {
    "Test <t@example.com> 1 +0000".to_string()
}

fn commit_req(message: &str) -> CommitRequest {
    let id = ident();
    CommitRequest {
        message: message.to_owned(),
        author: id.clone(),
        committer: id,
        allow_empty: false,
        sign_override: None,
        amend: false,
        assume_worktree_synced: true,
        skip_pre_commit_tree_match: false,
    }
}

/// Snapshot fixture shape: pre-staged path, tracked edit, and new untracked file.
#[test]
fn create_commit_change_count_includes_pre_staged_paths() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    fs::write(root.join("tracked.txt"), "v1\n").expect("write");
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("tracked.txt"), "modified\n").expect("mod");
    fs::write(root.join("staged.txt"), "staged\n").expect("staged");
    git(root, &["add", "staged.txt"]);
    fs::write(root.join("untracked.txt"), "new\n").expect("untracked");

    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage for commit");
    let outcome = create_commit(&repo, &commit_req("third"), &mut NullProgress).expect("commit");
    assert_eq!(
        outcome.changes, 3,
        "reported changes must include pre-staged, modified, and untracked paths"
    );
}

#[test]
fn stage_for_commit_stages_untracked_at_auto_cache_entry_threshold() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    let n = AUTO_UNTRACKED_CACHE_MIN_ENTRIES;
    for i in 0..n {
        let rel = format!("d{i:04}/f.txt");
        let path = root.join(&rel);
        fs::create_dir_all(path.parent().unwrap()).expect("mkdir");
        fs::write(&path, format!("{i}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "fill index"]);
    assert!(
        git(root, &["ls-files"]).lines().count() >= n,
        "fixture must reach UNTR auto-cache scale"
    );

    fs::write(root.join("d0000/f.txt"), "edited\n").expect("edit tracked");
    fs::write(root.join("brand-new.txt"), "fresh\n").expect("untracked");

    let repo = open_repo(root);
    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage");
    assert_eq!(outcome.modified, 1, "one tracked edit");
    assert_eq!(
        outcome.added, 1,
        "brand-new untracked must be staged at 4096+ scale"
    );
}

#[test]
fn allow_empty_still_stages_dirty_tracked_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);

    fs::write(root.join("a.txt"), "base\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("a.txt"), "dirty\n").expect("edit");

    let repo = open_repo(root);
    stage_for_commit(&repo, &mut NullProgress).expect("stage like commit");
    let mut req = commit_req("marker");
    req.allow_empty = true;
    req.skip_pre_commit_tree_match = true;
    let outcome = create_commit(&repo, &req, &mut NullProgress).expect("allow-empty after stage");
    assert_eq!(
        outcome.changes, 1,
        "staged edit must appear in the commit tree"
    );

    let porcelain = git(root, &["status", "--porcelain"]);
    assert!(
        porcelain.trim().is_empty(),
        "working tree should be clean after commit staged the edit, got:\n{porcelain}"
    );

    let _ = Command::new("git")
        .args(["fsck", "--no-progress"])
        .current_dir(root)
        .status()
        .expect("git fsck");
}

#[test]
fn stage_for_commit_stages_untracked_below_auto_cache_threshold() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    fs::write(root.join("tracked.txt"), "t\n").expect("write");
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "one"]);
    fs::write(root.join("tracked.txt"), "t2\n").expect("edit");
    fs::write(root.join("new.txt"), "n\n").expect("new");

    let repo = open_repo(root);
    let outcome = stage_for_commit(&repo, &mut NullProgress).expect("stage");
    assert_eq!(outcome.added, 1);
    assert_eq!(outcome.modified, 1);

    // Explicit `stage` with default options should behave the same for untracked.
    let repo2_root = tmp.path().join("small2");
    fs::create_dir_all(&repo2_root).expect("mkdir");
    git(&repo2_root, &["init", "-q", "-b", "main", "."]);
    fs::write(repo2_root.join("a.txt"), "a\n").expect("write");
    git(&repo2_root, &["add", "-A"]);
    git(&repo2_root, &["commit", "-qm", "one"]);
    fs::write(repo2_root.join("b.txt"), "b\n").expect("new");
    let repo2 = open_repo(&repo2_root);
    let out2 = stage(&repo2, &StageOptions::default(), &mut NullProgress).expect("stage");
    assert_eq!(out2.added, 1);
}
