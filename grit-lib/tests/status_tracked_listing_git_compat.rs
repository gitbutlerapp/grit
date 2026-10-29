//! Untracked and ignored paths from status match system `git` in directories that mostly
//! hold tracked files: tracked files are dropped while each directory is read, and a
//! directory's `.gitignore` is only read once a path inside it needs an ignore decision.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::status::{status, IgnoredMode, StatusOptions, UntrackedMode};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::Repository;
use grit_lib::repo_relpath::storage_bytes;
use grit_test_support::{forced_parallel_policy, git};

/// Tracked files spread over enough directories for the parallel walk to split them.
const DIRS: usize = 24;
const FILES_PER_DIR: usize = 50;

/// `(untracked, ignored)` raw paths from `git status --porcelain -z --ignored`.
fn git_lists(root: &Path, untracked_files: &str) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let out = Command::new("git")
        .current_dir(root)
        .args([
            "status",
            "--porcelain=v1",
            "-z",
            "--ignored",
            &format!("--untracked-files={untracked_files}"),
        ])
        .output()
        .expect("run git status");
    assert!(out.status.success(), "git status failed: {out:?}");
    let mut untracked = Vec::new();
    let mut ignored = Vec::new();
    for record in out.stdout.split(|&b| b == 0).filter(|r| !r.is_empty()) {
        if let Some(path) = record.strip_prefix(b"?? ") {
            untracked.push(path.to_vec());
        } else if let Some(path) = record.strip_prefix(b"!! ") {
            ignored.push(path.to_vec());
        }
    }
    untracked.sort();
    ignored.sort();
    (untracked, ignored)
}

/// `(untracked, ignored)` raw paths from [`status`] with traditional ignored listing.
fn grit_lists(repo: &Repository, mode: UntrackedMode) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let model = status(
        repo,
        &StatusOptions {
            untracked: mode,
            ignored: IgnoredMode::Traditional,
            ..StatusOptions::default()
        },
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    let raw = |paths: &[String]| {
        let mut out: Vec<Vec<u8>> = paths
            .iter()
            .map(|p| storage_bytes(p).into_owned())
            .collect();
        out.sort();
        out
    };
    (raw(&model.untracked), raw(&model.ignored))
}

fn open(root: &Path, parallel: bool) -> Repository {
    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open repo");
    if parallel {
        repo.with_thread_policy(forced_parallel_policy())
    } else {
        repo
    }
}

fn write(root: &Path, rel: &str, body: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
    fs::write(path, body).expect("write");
}

/// Mostly tracked tree with untracked and ignored paths placed where the listing filter and
/// the deferred `.gitignore` read could go wrong.
fn build_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "core.quotepath", "false"]);
    for d in 0..DIRS {
        for f in 0..FILES_PER_DIR {
            write(
                root,
                &format!("data/d{d:02}/f{f:03}.txt"),
                &format!("{d} {f}\n"),
            );
        }
    }
    // Names that sort around a tracked directory `a` (`a-b` < `a.txt` < `a/` < `a0`).
    write(root, "mix/a/inner.txt", "inner\n");
    write(root, "mix/a-b", "ab\n");
    write(root, "mix/a0", "a0\n");
    // A tracked file the leaf `.gitignore` would match stays tracked.
    write(root, "data/d03/keep.tmp", "tracked tmp\n");
    write(root, "data/d03/.gitignore", "*.tmp\nbuild/\n");
    write(root, "data/d07/.gitignore", "!*.log\n");
    write(root, ".gitignore", "*.log\n");
    git(root, &["add", "-A"]);
    git(root, &["add", "-f", "data/d03/keep.tmp"]);
    git(root, &["commit", "-qm", "base"]);

    // Untracked and ignored paths inside directories full of tracked files.
    write(root, "data/d00/new.txt", "new\n");
    write(root, "data/d03/scratch.tmp", "ignored by the leaf rule\n");
    write(root, "data/d03/build/out.o", "ignored dir\n");
    write(root, "data/d05/run.log", "ignored by the root rule\n");
    write(root, "data/d07/kept.log", "re-included by the leaf rule\n");
    write(root, "data/d09/sub/deep.txt", "untracked dir\n");
    write(root, "mix/a.txt", "between a-b and a/\n");
    write(root, "mix/a/inner2.txt", "untracked inside tracked dir\n");
    write(root, "top.txt", "untracked at the root\n");
}

fn assert_matches_git(root: &Path) {
    for (mode, flag) in [
        (UntrackedMode::Normal, "normal"),
        (UntrackedMode::All, "all"),
    ] {
        let want = git_lists(root, flag);
        for parallel in [false, true] {
            let got = grit_lists(&open(root, parallel), mode);
            assert_eq!(
                got, want,
                "untracked/ignored differ from git (--untracked-files={flag}, parallel={parallel})"
            );
        }
    }
}

#[test]
fn untracked_and_ignored_in_tracked_directories_match_git() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_repo(root);
    assert_matches_git(root);
}

#[test]
fn leaf_gitignore_added_after_commit_applies_to_new_untracked_files() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_repo(root);
    write(root, "data/d11/.gitignore", "*.bak\n");
    write(root, "data/d11/old.bak", "ignored\n");
    write(root, "data/d11/new.txt", "untracked\n");
    let (untracked, ignored) = grit_lists(&open(root, true), UntrackedMode::All);
    assert!(untracked.contains(&b"data/d11/new.txt".to_vec()));
    assert!(untracked.contains(&b"data/d11/.gitignore".to_vec()));
    assert!(ignored.contains(&b"data/d11/old.bak".to_vec()));
    assert_matches_git(root);
}

#[test]
fn conflict_only_paths_are_not_untracked() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    build_repo(root);
    git(root, &["checkout", "-qb", "side"]);
    write(root, "data/d02/both.txt", "side\n");
    git(root, &["add", "data/d02/both.txt"]);
    git(root, &["commit", "-qm", "side"]);
    git(root, &["checkout", "-q", "main"]);
    write(root, "data/d02/both.txt", "main\n");
    git(root, &["add", "data/d02/both.txt"]);
    git(root, &["commit", "-qm", "main"]);
    let merge = Command::new("git")
        .current_dir(root)
        .args(["merge", "-q", "side"])
        .output()
        .expect("run git merge");
    assert!(!merge.status.success(), "add/add merge must conflict");

    let (untracked, _) = grit_lists(&open(root, true), UntrackedMode::All);
    assert!(
        !untracked.contains(&b"data/d02/both.txt".to_vec()),
        "a path tracked only at conflict stages is not untracked: {untracked:?}"
    );
    assert_matches_git(root);
}
