//! `apply_stash_entry` / `pop_stash_entry` report the same conflicted paths system `git` sees.

use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::porcelain::stash::{
    apply_stash_entry, list_stashes, pop_stash_entry, StashApplyOutcome,
};
use grit_lib::repo::Repository;
use grit_test_support::git;

const IDENTITY: &str = "Test <t@example.com> 1000000000 +0000";

fn init_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
}

fn open_grit(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn write(root: &Path, path: &str, contents: &str) {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        std::fs::create_dir_all(parent).expect("mkdir");
    }
    std::fs::write(full, contents).expect("write");
}

/// Paths `git ls-files -u` reports as unmerged, sorted and de-duplicated.
fn git_unmerged(root: &Path) -> Vec<String> {
    let mut paths: Vec<String> = git(root, &["ls-files", "-u"])
        .lines()
        .filter_map(|line| line.split_once('\t').map(|(_, p)| p.to_owned()))
        .collect();
    paths.dedup();
    paths
}

fn copy_repo(from: &Path, to: &Path) {
    for entry in walk(from) {
        let rel = entry.strip_prefix(from).expect("prefix");
        let dest = to.join(rel);
        if entry.is_dir() {
            std::fs::create_dir_all(&dest).expect("mkdir");
        } else {
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent).expect("mkdir");
            }
            std::fs::copy(&entry, &dest).expect("copy");
        }
    }
}

fn walk(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).expect("read_dir") {
        let path = entry.expect("entry").path();
        out.push(path.clone());
        if path.is_dir() {
            out.extend(walk(&path));
        }
    }
    out
}

/// `a.txt` and `dir/b.txt` are changed both in the stash and on a newer HEAD, so applying
/// the stash conflicts on both; `clean.txt` only changes in the stash.
fn conflicting_stash(root: &Path) {
    init_repo(root);
    write(root, "a.txt", "base a\n");
    write(root, "dir/b.txt", "base b\n");
    write(root, "clean.txt", "base clean\n");
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "init"]);

    write(root, "a.txt", "stashed a\n");
    write(root, "dir/b.txt", "stashed b\n");
    write(root, "clean.txt", "stashed clean\n");
    git(root, &["stash", "push", "-q", "-m", "conflicting"]);

    write(root, "a.txt", "head a\n");
    write(root, "dir/b.txt", "head b\n");
    git(root, &["commit", "-qam", "move head"]);
}

#[test]
fn apply_entry_reports_conflicts_git_also_sees() {
    let dir = tempfile::tempdir().expect("tempdir");
    conflicting_stash(dir.path());
    let reference = tempfile::tempdir().expect("tempdir");
    copy_repo(dir.path(), reference.path());

    let repo = open_grit(dir.path());
    let outcome = apply_stash_entry(&repo, dir.path(), 0, false).expect("apply");
    assert!(outcome.has_conflicts());
    assert_eq!(outcome.conflicted_paths, ["a.txt", "dir/b.txt"]);

    // git reads the index grit wrote and sees the same unmerged paths.
    assert_eq!(git_unmerged(dir.path()), outcome.conflicted_paths);
    // system git applying the same stash leaves the same paths unmerged.
    let _ = grit_test_support::git_cmd(&["stash", "apply", "-q"])
        .in_dir(reference.path())
        .exec();
    assert_eq!(git_unmerged(reference.path()), outcome.conflicted_paths);

    assert_eq!(
        std::fs::read_to_string(dir.path().join("clean.txt")).expect("read"),
        "stashed clean\n"
    );
    assert_eq!(
        list_stashes(&repo).expect("list").len(),
        1,
        "apply keeps the entry"
    );
}

#[test]
fn pop_entry_with_conflicts_keeps_entry() {
    let dir = tempfile::tempdir().expect("tempdir");
    conflicting_stash(dir.path());
    let repo = open_grit(dir.path());

    let outcome = pop_stash_entry(&repo, dir.path(), 0, false, IDENTITY).expect("pop");
    assert_eq!(outcome.conflicted_paths, ["a.txt", "dir/b.txt"]);
    assert_eq!(list_stashes(&repo).expect("list").len(), 1);
    assert_eq!(git(dir.path(), &["stash", "list"]).lines().count(), 1);
}

#[test]
fn clean_apply_and_pop_report_no_conflicts() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    write(dir.path(), "f.txt", "base\n");
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-qm", "init"]);
    write(dir.path(), "f.txt", "older\n");
    git(dir.path(), &["stash", "push", "-q", "-m", "older"]);
    write(dir.path(), "f.txt", "newer\n");
    git(dir.path(), &["stash", "push", "-q", "-m", "newer"]);
    let repo = open_grit(dir.path());

    let applied = apply_stash_entry(&repo, dir.path(), 1, false).expect("apply older");
    assert_eq!(applied, StashApplyOutcome::default());
    assert!(!applied.has_conflicts());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
        "older\n"
    );
    assert_eq!(list_stashes(&repo).expect("list").len(), 2);

    git(dir.path(), &["checkout", "-q", "--", "f.txt"]);
    let popped = pop_stash_entry(&repo, dir.path(), 0, false, IDENTITY).expect("pop newer");
    assert!(!popped.has_conflicts());
    assert_eq!(
        std::fs::read_to_string(dir.path().join("f.txt")).expect("read"),
        "newer\n"
    );
    let remaining = git(dir.path(), &["stash", "list"]);
    assert_eq!(remaining.lines().count(), 1, "{remaining}");
    assert!(remaining.contains("older"), "{remaining}");
    assert!(git_unmerged(dir.path()).is_empty());
    git(dir.path(), &["fsck", "--strict"]);
}

#[test]
fn entry_out_of_range_is_stash_not_found() {
    let dir = tempfile::tempdir().expect("tempdir");
    init_repo(dir.path());
    git(dir.path(), &["commit", "-q", "--allow-empty", "-m", "init"]);
    let repo = open_grit(dir.path());

    let err = apply_stash_entry(&repo, dir.path(), 0, false).unwrap_err();
    assert!(matches!(err, Error::StashNotFound { n: 0 }), "{err:?}");
    let err = pop_stash_entry(&repo, dir.path(), 3, false, IDENTITY).unwrap_err();
    assert!(matches!(err, Error::StashNotFound { n: 3 }), "{err:?}");
}
