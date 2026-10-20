//! `Diff::has_changes` matches system `git diff --exit-code` semantics (t4017).

use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{DiffOptions, DiffTarget, WORKTREE_HAS_CHANGES_ENTRIES_SCANNED};
use grit_lib::repo::Repository;
use grit_test_support::git;

fn open_repo(root: &std::path::Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn git_diff_exit(root: &std::path::Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .expect("git diff")
        .code()
        .unwrap_or(1)
}

fn grit_has_changes(repo: &Repository, target: DiffTarget, paths: &[String]) -> bool {
    repo.diff()
        .has_changes(target, paths, DiffOptions::default())
        .expect("has_changes")
}

fn assert_matches_git_exit(
    root: &std::path::Path,
    repo: &Repository,
    git_args: &[&str],
    target: DiffTarget,
    paths: &[String],
) {
    let git_code = git_diff_exit(root, git_args);
    let grit_changed = grit_has_changes(repo, target, paths);
    let git_changed = git_code == 1;
    assert_eq!(
        grit_changed, git_changed,
        "git diff {:?} exited {git_code}, grit has_changes={grit_changed}",
        git_args
    );
}

#[test]
fn has_changes_matches_git_exit_code_clean_uncommitted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "v\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    let repo = open_repo(root);
    assert_matches_git_exit(
        root,
        &repo,
        &["diff", "--exit-code"],
        DiffTarget::Uncommitted,
        &[],
    );
}

#[test]
fn has_changes_matches_git_exit_code_dirty_uncommitted() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "v\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("a.txt"), "dirty\n").expect("write");
    let repo = open_repo(root);
    assert_matches_git_exit(
        root,
        &repo,
        &["diff", "--exit-code"],
        DiffTarget::Uncommitted,
        &[],
    );
}

#[test]
fn has_changes_matches_git_exit_code_staged_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "v\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("a.txt"), "staged\n").expect("write");
    git(root, &["add", "a.txt"]);
    let repo = open_repo(root);
    assert_matches_git_exit(
        root,
        &repo,
        &["diff", "--cached", "--exit-code"],
        DiffTarget::Staged,
        &[],
    );
}

#[test]
fn has_changes_matches_git_exit_code_unstaged_only() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "v\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("a.txt"), "wt\n").expect("write");
    let repo = open_repo(root);
    let git_changed = !Command::new("git")
        .args(["-C", root.to_str().unwrap(), "diff-files", "--quiet"])
        .status()
        .expect("git diff-files")
        .success();
    let grit_changed = grit_has_changes(&repo, DiffTarget::Unstaged, &[]);
    assert_eq!(grit_changed, git_changed);
}

#[test]
fn has_changes_matches_git_exit_code_path_limited() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "a\n").expect("write");
    std::fs::write(root.join("b.txt"), "b\n").expect("write");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("a.txt"), "a2\n").expect("write");
    let repo = open_repo(root);
    let paths = vec!["b.txt".to_owned()];
    assert_matches_git_exit(
        root,
        &repo,
        &["diff", "--exit-code", "--", "b.txt"],
        DiffTarget::Uncommitted,
        &paths,
    );
}

#[test]
fn has_changes_matches_git_exit_code_mode_only_respects_filemode() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.filemode", "true"]);
    std::fs::write(root.join("a.txt"), "x\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    git(root, &["update-index", "--chmod=+x", "a.txt"]);
    let repo = open_repo(root);
    assert_matches_git_exit(
        root,
        &repo,
        &["diff", "--cached", "--exit-code"],
        DiffTarget::Staged,
        &[],
    );
}

#[test]
fn has_changes_matches_git_exit_code_whitespace_only_with_ignore_all_space() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "hello\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("a.txt"), "hel lo\n").expect("write");
    git(root, &["add", "a.txt"]);
    let repo = open_repo(root);
    let git_code = git_diff_exit(root, &["diff", "--cached", "-w", "--exit-code"]);
    let mut opts = DiffOptions::default();
    opts.ignore_all_space = true;
    let grit_changed = repo
        .diff()
        .has_changes(DiffTarget::Staged, &[], opts)
        .expect("has_changes");
    assert_eq!(grit_changed, git_code == 1);
}

#[test]
fn has_changes_racy_stat_identical_content_is_clean() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a.txt"), "same\n").expect("write");
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "init"]);
    let path = root.join("a.txt");
    let meta = std::fs::metadata(&path).expect("meta");
    let mtime = filetime::FileTime::from_last_modification_time(&meta);
    filetime::set_file_mtime(&path, mtime).expect("touch");
    let repo = open_repo(root);
    assert_matches_git_exit(
        root,
        &repo,
        &["diff", "--exit-code"],
        DiffTarget::Unstaged,
        &[],
    );
}

#[test]
fn has_changes_short_circuits_after_first_changed_entry() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    for name in ["a.txt", "b.txt", "c.txt"] {
        std::fs::write(root.join(name), format!("{name}\n")).expect("write");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);
    std::fs::write(root.join("a.txt"), "a-dirty\n").expect("write");
    let repo = open_repo(root);
    WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.store(0, std::sync::atomic::Ordering::SeqCst);
    assert!(repo
        .diff()
        .has_changes(DiffTarget::Unstaged, &[], DiffOptions::default())
        .expect("has_changes"));
    let scanned = WORKTREE_HAS_CHANGES_ENTRIES_SCANNED.load(std::sync::atomic::Ordering::SeqCst);
    assert!(
        scanned <= 1,
        "expected early stop after first changed file, scanned {scanned} entries"
    );
}
