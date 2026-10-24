//! After `git clone`, grit must trust Git-written index stat data (issue #1304).
//!
//! On Windows with `core.autocrlf=true`, directory-scan metadata can disagree with the
//! path-based stats Git stored in the index; the first `grit status` must not re-hash the tree.

use std::fs;
use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::stat_matches;
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

fn assert_git_index_stats_match_worktree(repo_root: &Path) {
    let index = grit_lib::index::Index::load(&repo_root.join(".git/index")).expect("load index");
    for entry in index.entries() {
        if entry.stage() != 0 {
            continue;
        }
        let rel = std::str::from_utf8(&entry.path).expect("utf-8 path");
        let meta = fs::symlink_metadata(repo_root.join(rel)).expect("worktree metadata");
        assert!(
            stat_matches(entry, &meta),
            "git-written index stat must match symlink_metadata for {rel} before grit status"
        );
    }
}

#[test]
fn git_clone_index_stats_match_worktree_with_autocrlf() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    const FILE_COUNT: usize = 512;
    init_autocrlf_repo(&src, FILE_COUNT);

    let clone_root = tmp.path().join("clone");
    let src_url = format!(
        "file://{}",
        src.canonicalize().expect("canonical src").display()
    );
    let clone_exit = Command::new("git")
        .args(["clone", "-q", "--no-hardlinks", &src_url])
        .arg(&clone_root)
        .status()
        .expect("git clone");
    assert!(clone_exit.success(), "git clone fixture repo");
    git(&clone_root, &["config", "core.autocrlf", "true"]);

    assert_git_index_stats_match_worktree(&clone_root);

    let grit_repo = Repository::open(
        &clone_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&clone_root),
    )
    .expect("open grit repo");
    let index_path = grit_repo.index_handle().path();
    let index_mtime_before = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");

    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("first status after git clone");
    assert!(
        model.unstaged.is_empty() && model.untracked.is_empty(),
        "fresh git clone must be clean in grit status"
    );

    let index_mtime_after = fs::metadata(&index_path)
        .expect("index metadata")
        .modified()
        .expect("index mtime");
    assert_eq!(
        index_mtime_after, index_mtime_before,
        "clean first status after git clone must not rewrite the index"
    );

    let git_diff = git(&clone_root, &["diff-files"]);
    assert!(
        git_diff.trim().is_empty(),
        "system git diff-files must stay clean after grit status on git clone index"
    );
}

#[test]
fn first_status_after_git_clone_is_bounded_with_autocrlf() {
    if std::env::var_os("CARGO_LLVM_COV").is_some() {
        eprintln!("SKIP: timing oracle under llvm-cov instrumentation");
        return;
    }
    let tmp = tempfile::tempdir().expect("tempdir");
    let src = tmp.path().join("src");
    fs::create_dir_all(&src).expect("mkdir src");
    let file_count = if cfg!(windows) { 800 } else { 400 };
    init_autocrlf_repo(&src, file_count);

    let clone_root = tmp.path().join("clone");
    let src_url = format!(
        "file://{}",
        src.canonicalize().expect("canonical src").display()
    );
    let clone_status = Command::new("git")
        .args(["clone", "-q", "--no-hardlinks", &src_url])
        .arg(&clone_root)
        .status()
        .expect("git clone");
    assert!(clone_status.success(), "git clone fixture repo");
    git(&clone_root, &["config", "core.autocrlf", "true"]);

    let grit_repo = Repository::open(
        &clone_root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&clone_root),
    )
    .expect("open grit repo");

    let start = Instant::now();
    let model = status(
        &grit_repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("first status after git clone");
    let elapsed = start.elapsed();
    assert!(model.unstaged.is_empty() && model.untracked.is_empty());

    let budget = if cfg!(windows) {
        Duration::from_secs(3)
    } else {
        Duration::from_secs(2)
    };
    assert!(
        elapsed < budget,
        "first status after git clone should trust git index stats (got {elapsed:?}, budget {budget:?})"
    );
}
