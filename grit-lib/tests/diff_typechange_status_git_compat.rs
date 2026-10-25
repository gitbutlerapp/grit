//! One status rule for file <-> symlink changes in every diff mode (issue #1843).
//!
//! A path whose object type changes (regular file vs symlink) is
//! [`DiffStatus::TypeChanged`] (Git raw status `T`) whether the diff compares
//! work tree vs index, index vs `HEAD`, work tree vs a tree, or two trees. A
//! permission-only change stays [`DiffStatus::Modified`]. Each case is checked
//! against the system `git diff --raw`.
#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_tree_to_worktree, diff_trees,
    format_raw, DiffEntry, DiffIndexToWorktreeOptions, DiffOptions, DiffStatus, DiffTarget,
};
use grit_lib::objects::ObjectId;
use grit_lib::repo::Repository;
use grit_test_support::git;
use tempfile::TempDir;

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "u@example.com"]);
    git(dir, &["config", "user.name", "U"]);
}

fn commit_all(dir: &Path, msg: &str) -> ObjectId {
    git(dir, &["add", "-A"]);
    git(dir, &["commit", "-qm", msg]);
    git(dir, &["rev-parse", "HEAD"])
        .trim()
        .parse()
        .expect("oid")
}

fn head_tree(dir: &Path, rev: &str) -> ObjectId {
    git(dir, &["rev-parse", &format!("{rev}^{{tree}}")])
        .trim()
        .parse()
        .expect("tree oid")
}

fn open_repo(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn git_raw(dir: &Path, args: &[&str]) -> Vec<String> {
    let mut cmd_args = vec!["-c", "core.abbrev=40", "diff", "--raw", "--no-renames"];
    cmd_args.extend(args);
    git(dir, &cmd_args)
        .lines()
        .filter(|l| l.starts_with(':'))
        .map(str::to_owned)
        .collect()
}

/// Raw lines for `target` through [`grit_lib::diff::Diff::files`], the API `grit diff` uses.
fn grit_raw(repo: &Repository, target: &DiffTarget) -> Vec<String> {
    let mut opts = DiffOptions::default();
    opts.word_level_inline = false;
    repo.diff()
        .files(target, &[], &opts)
        .expect("files")
        .into_iter()
        .map(|f| {
            format_raw(&DiffEntry {
                status: f.status,
                old_path: Some(f.path.clone()),
                new_path: Some(f.path),
                old_mode: f.old_mode.unwrap_or_else(|| "000000".into()),
                new_mode: f.new_mode.unwrap_or_else(|| "000000".into()),
                old_oid: f.old_oid,
                new_oid: f.new_oid,
                score: None,
            })
        })
        .collect()
}

/// Drops the new-side object id, which `git diff --raw` leaves null for unhashed work tree files.
fn without_worktree_oid(lines: Vec<String>) -> Vec<String> {
    lines
        .into_iter()
        .map(|line| {
            let mut fields: Vec<&str> = line.splitn(5, ' ').collect();
            fields.remove(3);
            fields.join(" ")
        })
        .collect()
}

fn statuses(entries: &[DiffEntry]) -> Vec<(String, DiffStatus)> {
    entries
        .iter()
        .map(|e| (e.path().to_owned(), e.status))
        .collect()
}

fn single(path: &str, status: DiffStatus) -> Vec<(String, DiffStatus)> {
    vec![(path.to_owned(), status)]
}

/// Asserts every diff mode reports `expected` for `path` and matches `git diff --raw`.
///
/// The repository must have the change in the work tree only (nothing staged).
/// The check stages it with `git add`, then commits it, and compares each step.
fn assert_all_modes(dir: &Path, path: &str, expected: DiffStatus) {
    let base = git(dir, &["rev-parse", "HEAD"]);
    let base = base.trim();
    let base_tree = head_tree(dir, "HEAD");
    let letter = expected.letter();

    let repo = open_repo(dir);
    let unstaged = without_worktree_oid(grit_raw(&repo, &DiffTarget::WorktreeVsIndex));
    assert_eq!(
        unstaged,
        without_worktree_oid(git_raw(dir, &[])),
        "work tree vs index"
    );
    assert!(
        unstaged[0].contains(&format!(" {letter}\t")),
        "{unstaged:?}"
    );
    let vs_head = without_worktree_oid(grit_raw(&repo, &DiffTarget::WorktreeVsHead));
    assert_eq!(
        vs_head,
        without_worktree_oid(git_raw(dir, &["HEAD"])),
        "work tree vs HEAD"
    );

    let mut index = repo.index_handle().load().expect("index");
    let (entries, _) = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        dir,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("index vs worktree");
    assert_eq!(statuses(&entries), single(path, expected));
    let entries = diff_index_to_worktree_with_options(
        repo.odb(),
        &mut index,
        dir,
        DiffIndexToWorktreeOptions {
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            for_status: true,
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .expect("index vs worktree for status")
    .0;
    assert_eq!(statuses(&entries), single(path, expected));
    let entries =
        diff_tree_to_worktree(repo.odb(), Some(&base_tree), dir, &index).expect("tree vs wt");
    assert_eq!(statuses(&entries), single(path, expected));

    git(dir, &["add", "-A"]);
    let repo = open_repo(dir);
    let staged = grit_raw(&repo, &DiffTarget::IndexVsHead);
    assert_eq!(staged, git_raw(dir, &["--cached"]), "index vs HEAD");
    assert!(staged[0].contains(&format!(" {letter}\t")), "{staged:?}");
    let vs_head = without_worktree_oid(grit_raw(&repo, &DiffTarget::WorktreeVsHead));
    assert_eq!(
        vs_head,
        without_worktree_oid(git_raw(dir, &["HEAD"])),
        "staged: work tree vs HEAD"
    );
    let index = repo.index_handle().load().expect("index");
    let entries = diff_index_to_tree(repo.odb(), &index, Some(&base_tree), false).expect("i vs t");
    assert_eq!(statuses(&entries), single(path, expected));
    let entries =
        diff_tree_to_worktree(repo.odb(), Some(&base_tree), dir, &index).expect("tree vs wt");
    assert_eq!(statuses(&entries), single(path, expected));

    let tip = commit_all(dir, "change");
    let repo = open_repo(dir);
    let base_oid: ObjectId = base.parse().expect("oid");
    let trees = grit_raw(
        &repo,
        &DiffTarget::Trees {
            old: Some(base_oid),
            new: tip,
        },
    );
    assert_eq!(trees, git_raw(dir, &[base, &tip.to_hex()]), "tree vs tree");
    assert_eq!(
        trees, staged,
        "same raw line for staged and committed change"
    );
    let tip_tree = head_tree(dir, "HEAD");
    let entries = diff_trees(repo.odb(), Some(&base_tree), Some(&tip_tree), "").expect("trees");
    assert_eq!(statuses(&entries), single(path, expected));
}

#[test]
fn file_replaced_by_symlink_is_type_changed_everywhere() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f.txt"), "t\n").expect("write");
    std::fs::write(dir.path().join("target.txt"), "c\n").expect("write");
    commit_all(dir.path(), "a");

    std::fs::remove_file(dir.path().join("f.txt")).expect("rm");
    symlink("target.txt", dir.path().join("f.txt")).expect("symlink");

    assert_all_modes(dir.path(), "f.txt", DiffStatus::TypeChanged);
}

#[test]
fn symlink_replaced_by_file_is_type_changed_everywhere() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("target.txt"), "c\n").expect("write");
    symlink("target.txt", dir.path().join("link")).expect("symlink");
    commit_all(dir.path(), "a");

    std::fs::remove_file(dir.path().join("link")).expect("rm");
    std::fs::write(dir.path().join("link"), "now a regular file\n").expect("write");

    assert_all_modes(dir.path(), "link", DiffStatus::TypeChanged);
}

#[test]
fn symlink_with_same_bytes_as_file_is_type_changed() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("f"), "target.txt").expect("write");
    commit_all(dir.path(), "a");

    // Same blob id on both sides: only the mode type bits differ.
    std::fs::remove_file(dir.path().join("f")).expect("rm");
    symlink("target.txt", dir.path().join("f")).expect("symlink");

    assert_all_modes(dir.path(), "f", DiffStatus::TypeChanged);
}

#[test]
fn chmod_only_stays_modified_everywhere() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("run.sh"), "echo hi\n").expect("write");
    commit_all(dir.path(), "a");

    let path = dir.path().join("run.sh");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");

    assert_all_modes(dir.path(), "run.sh", DiffStatus::Modified);
}

#[test]
fn chmod_and_content_change_stays_modified_everywhere() {
    let dir = TempDir::new().expect("tempdir");
    init_repo(dir.path());
    std::fs::write(dir.path().join("run.sh"), "echo hi\n").expect("write");
    commit_all(dir.path(), "a");

    let path = dir.path().join("run.sh");
    std::fs::write(&path, "echo bye\n").expect("write");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");

    assert_all_modes(dir.path(), "run.sh", DiffStatus::Modified);
}
