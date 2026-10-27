//! Issue #1682: under `core.ignorecase`, `stage` records case-only renames of nested
//! directories and files with one directory listing per directory, and system `git` reads
//! the resulting index and commit as clean.

use std::fs;
use std::path::Path;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions, StageOutcome};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

const TRACKED: [&str; 4] = [
    "Dir/Sub/a.txt",
    "Dir/Sub/b.txt",
    "Dir/top.txt",
    "other/Readme.md",
];

/// A git-made repository with `core.ignorecase=true` whose tree holds [`TRACKED`].
fn ignorecase_repo(root: &Path) {
    git(root, &["init", "-q", "-b", "main", "."]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "core.ignorecase", "true"]);
    for path in TRACKED {
        let abs = root.join(path);
        fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
        fs::write(&abs, format!("{path}\n")).expect("write");
    }
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "base"]);
}

fn open(root: &Path) -> Repository {
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open")
}

fn stage_with(root: &Path, mode: StageMode, dry_run: bool) -> StageOutcome {
    let opts = StageOptions {
        mode,
        dry_run,
        ..StageOptions::default()
    };
    stage(&open(root), &opts, &mut NullProgress).expect("stage")
}

fn index_paths(root: &Path) -> Vec<String> {
    let index = Index::load(&root.join(".git/index")).expect("index");
    let mut paths: Vec<String> = index
        .entries()
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect();
    paths.sort();
    paths
}

/// Rename the `Dir` directory to `dir` and `other/Readme.md` to `other/README.md`.
fn rename_case_only(root: &Path) {
    fs::rename(root.join("Dir"), root.join("dir")).expect("rename dir");
    fs::rename(root.join("other/Readme.md"), root.join("other/README.md")).expect("rename file");
}

const RESPELLED: [&str; 4] = [
    "dir/Sub/a.txt",
    "dir/Sub/b.txt",
    "dir/top.txt",
    "other/README.md",
];

fn assert_git_sees_clean_respelled_commit(root: &Path) {
    git(root, &["commit", "-qm", "respell"]);
    git(root, &["fsck", "--strict"]);
    let listed = git(root, &["ls-files"]);
    assert_eq!(listed.lines().collect::<Vec<_>>(), RESPELLED);
    let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert_eq!(tree.lines().collect::<Vec<_>>(), RESPELLED);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}

#[test]
fn stage_all_records_case_only_renames_of_nested_directories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    ignorecase_repo(root);
    rename_case_only(root);

    let outcome = stage_with(root, StageMode::All, false);
    for path in RESPELLED {
        assert!(
            outcome.modified.iter().any(|p| p == path),
            "{path} should be reported as respelled: {outcome:?}"
        );
    }
    assert!(outcome.added.is_empty(), "{outcome:?}");
    assert_eq!(index_paths(root), RESPELLED);
    assert_git_sees_clean_respelled_commit(root);
}

#[test]
fn stage_update_records_case_only_renames_of_nested_directories() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    ignorecase_repo(root);
    rename_case_only(root);

    stage_with(root, StageMode::Update, false);
    assert_eq!(index_paths(root), RESPELLED);
    assert_git_sees_clean_respelled_commit(root);
}

#[test]
fn dry_run_reports_respelled_paths_without_touching_the_index() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    ignorecase_repo(root);
    let before = fs::read(root.join(".git/index")).expect("index");
    rename_case_only(root);

    let outcome = stage_with(root, StageMode::All, true);
    for path in RESPELLED {
        assert!(
            outcome.modified.iter().any(|p| p == path),
            "dry run should report {path}: {outcome:?}"
        );
    }
    assert_eq!(fs::read(root.join(".git/index")).expect("index"), before);
}

#[test]
fn unrenamed_tree_stages_nothing_under_ignorecase() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    ignorecase_repo(root);

    let outcome = stage_with(root, StageMode::All, false);
    assert_eq!(outcome.total(), 0, "{outcome:?}");
    let mut expected = TRACKED.to_vec();
    expected.sort_unstable();
    assert_eq!(index_paths(root), expected);
    assert_eq!(git(root, &["status", "--porcelain"]), "");
}
