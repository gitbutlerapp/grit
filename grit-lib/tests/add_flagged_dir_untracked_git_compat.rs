//! Pathless `git add` must stage untracked siblings beside assume-unchanged / skip-worktree files.

mod support;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use std::fs;

use support::git;

fn seed_repo(root: &std::path::Path) {
    fs::create_dir_all(root.join("foo")).unwrap();
    fs::write(root.join("foo/tracked.txt"), b"tracked\n").unwrap();
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["add", "foo/tracked.txt"]);
    git(root, &["commit", "-qm", "seed"]);
}

fn grit_add_all(root: &std::path::Path) -> usize {
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let outcome = stage(&repo, &StageOptions::all(), &mut NullProgress).expect("add all");
    outcome.total()
}

#[test]
fn pathless_add_stages_untracked_beside_assume_unchanged() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    seed_repo(root);
    git(
        root,
        &["update-index", "--assume-unchanged", "foo/tracked.txt"],
    );
    fs::write(root.join("foo/sibling.txt"), b"new\n").unwrap();

    let staged = grit_add_all(root);
    assert!(
        staged >= 1,
        "pathless add must stage untracked sibling beside assume-unchanged tracked file"
    );
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"foo/sibling.txt", 0).is_some(),
        "index must contain foo/sibling.txt"
    );

    git(root, &["add", "-A"]);
    let out = git(root, &["diff", "--cached", "--name-only"]);
    assert!(
        out.lines().any(|l| l == "foo/sibling.txt"),
        "git add -A must stage sibling for comparison: {out:?}"
    );
}

#[test]
fn pathless_add_stages_untracked_beside_skip_worktree() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    seed_repo(root);
    git(
        root,
        &["update-index", "--skip-worktree", "foo/tracked.txt"],
    );
    fs::write(root.join("foo/sibling.txt"), b"new\n").unwrap();

    let staged = grit_add_all(root);
    assert!(
        staged >= 1,
        "pathless add must stage untracked sibling beside skip-worktree tracked file"
    );
    let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("open");
    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"foo/sibling.txt", 0).is_some(),
        "index must contain foo/sibling.txt"
    );

    git(root, &["add", "-A"]);
    let out = git(root, &["diff", "--cached", "--name-only"]);
    assert!(
        out.lines().any(|l| l == "foo/sibling.txt"),
        "git add -A must stage sibling for comparison: {out:?}"
    );
}
