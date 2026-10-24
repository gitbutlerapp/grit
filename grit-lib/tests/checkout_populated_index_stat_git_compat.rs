//! Populated checkout must write index stat data that matches status probes (issue #1032).
//!
//! After `grit clone`, the first `grit status` must not re-hash the whole tree because checkout
//! recorded stale stat fields. On Windows with `core.autocrlf=true`, open-handle metadata differs
//! from path-based `symlink_metadata`, so checkout must stat by path after flush/close.

use std::fs;
use std::path::Path;
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::stat_matches;
use grit_lib::porcelain::checkout::checkout_between_trees;
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_autocrlf_repo(root: &Path, file_count: usize) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.autocrlf", "true"]);
    for i in 0..file_count {
        let name = format!("file-{i:04}.txt");
        fs::write(root.join(&name), format!("line {i}\n")).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
}

fn tree_of_head(root: &Path) -> grit_lib::objects::ObjectId {
    let hex = git(root, &["rev-parse", "HEAD^{tree}"]);
    grit_lib::objects::ObjectId::from_hex(hex.trim()).expect("tree oid")
}

fn assert_index_stats_match_worktree(repo_root: &Path, repo: &Repository) {
    let index = repo.index_handle().load().expect("index");
    for entry in index.entries() {
        if entry.stage() != 0 {
            continue;
        }
        let rel = std::str::from_utf8(&entry.path).expect("utf-8 path");
        let meta = fs::symlink_metadata(repo_root.join(rel)).expect("worktree metadata");
        assert!(
            stat_matches(entry, &meta),
            "checkout index stat must match symlink_metadata for {rel} (issue #1032)"
        );
    }
}

#[test]
fn populated_checkout_index_stat_matches_worktree_with_autocrlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    const FILE_COUNT: usize = 256;
    init_autocrlf_repo(repo_root, FILE_COUNT);
    let tree = tree_of_head(repo_root);

    let repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");

    let mut index = grit_lib::index::Index::new();
    repo.index_handle().write(&mut index).expect("clear index");
    for i in 0..FILE_COUNT {
        let name = format!("file-{i:04}.txt");
        let _ = fs::remove_file(repo_root.join(&name));
    }

    checkout_between_trees(&repo, None, &tree).expect("populated checkout");
    assert_index_stats_match_worktree(repo_root, &repo);

    let git_diff = git(repo_root, &["diff-files"]);
    assert!(
        git_diff.trim().is_empty(),
        "system git diff-files must be clean immediately after populated checkout"
    );
}

#[test]
fn first_status_after_populated_checkout_does_not_rewrite_warm_index() {
    if cfg!(coverage) || std::env::var_os("CARGO_LLVM_COV").is_some() {
        eprintln!("SKIP: timing oracle under llvm-cov instrumentation");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo_root = tmp.path();
    let file_count = if cfg!(windows) { 800 } else { 400 };
    init_autocrlf_repo(repo_root, file_count);
    let tree = tree_of_head(repo_root);

    let repo = Repository::open(
        &repo_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(repo_root),
    )
    .expect("open grit repo");

    let mut index = grit_lib::index::Index::new();
    repo.index_handle().write(&mut index).expect("clear index");
    for i in 0..file_count {
        let name = format!("file-{i:04}.txt");
        let _ = fs::remove_file(repo_root.join(&name));
    }
    checkout_between_trees(&repo, None, &tree).expect("populated checkout");
    assert_index_stats_match_worktree(repo_root, &repo);

    let index_path = repo.index_handle().path();
    let opts = StatusOptions::default();

    let first_start = Instant::now();
    let first = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("first status");
    let first_elapsed = first_start.elapsed();
    assert!(
        first.unstaged.is_empty(),
        "populated checkout must be clean"
    );
    assert!(first.untracked.is_empty());

    let index_mtime_after_first = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let second_start = Instant::now();
    let second = status(&repo, &opts, &mut NullProgress, &NeverCancel).expect("second status");
    let second_elapsed = second_start.elapsed();
    assert!(second.unstaged.is_empty());

    let index_mtime_after_second = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert_eq!(
        index_mtime_after_second, index_mtime_after_first,
        "second clean status must not rewrite the index"
    );

    assert!(
        second_elapsed <= first_elapsed + Duration::from_millis(10),
        "two clean status runs should do comparable work (first {first_elapsed:?}, second {second_elapsed:?})"
    );
}
