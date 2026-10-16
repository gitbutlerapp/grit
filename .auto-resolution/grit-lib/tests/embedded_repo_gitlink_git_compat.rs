//! Issue #1006: nested `git init` under a grit repo must stage as gitlink, not flat blobs.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::MODE_GITLINK;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::git;

fn init_grit_repo(root: &std::path::Path) -> Repository {
    fs::create_dir_all(root.join(".git/objects")).expect("objects");
    fs::create_dir_all(root.join(".git/refs/heads")).expect("refs");
    fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("head");
    fs::write(
        root.join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
    )
    .expect("config");
    Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open grit repo")
}

fn init_nested_git_repo(inner: &std::path::Path) {
    git(inner, &["init", "-q", "-b", "main"]);
    git(inner, &["config", "user.email", "t@example.com"]);
    git(inner, &["config", "user.name", "Test"]);
    fs::write(inner.join("i"), b"i\n").expect("write inner file");
    git(inner, &["add", "i"]);
    git(inner, &["commit", "-qm", "i"]);
}

#[test]
fn add_stages_unregistered_embedded_repo_as_gitlink() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    fs::write(root.join("a"), b"a\n").expect("write a");

    let inner = root.join("inner");
    fs::create_dir_all(&inner).expect("mkdir inner");
    init_nested_git_repo(&inner);

    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add all");

    let index = repo.index_handle().load().expect("index");
    let entry = index
        .get(b"inner", 0)
        .expect("index must contain inner gitlink, not inner/i blob");
    assert_eq!(entry.mode, MODE_GITLINK);
    assert!(
        index.get(b"inner/i", 0).is_none(),
        "inner repository files must not be indexed in the outer repo"
    );

    let grit_ls = git(root, &["ls-files", "-s"]);
    assert!(
        grit_ls.contains("160000") && grit_ls.contains("\tinner\n"),
        "git ls-files after grit add: {grit_ls}"
    );
    assert!(
        !grit_ls.contains("inner/i"),
        "git must not see inner/i as tracked: {grit_ls}"
    );
}

#[test]
fn add_skips_gitignored_embedded_repo() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    fs::write(root.join(".gitignore"), "inner/\n").expect("gitignore");
    git(root, &["add", ".gitignore"]);
    git(root, &["commit", "-qm", "ignore inner"]);

    let inner = root.join("inner");
    fs::create_dir_all(&inner).expect("mkdir inner");
    init_nested_git_repo(&inner);

    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add all");

    let grit_ls = git(root, &["ls-files", "-s"]);
    assert!(
        !grit_ls.contains("\tinner\n") && !grit_ls.contains("160000"),
        "gitignored embedded repo must not be staged: {grit_ls}"
    );
    assert!(
        grit_ls.contains(".gitignore"),
        "expected only tracked .gitignore, got: {grit_ls}"
    );
}

#[test]
fn add_does_not_treat_placeholder_dot_git_as_embedded_repo() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    fs::create_dir_all(root.join("inner/.git")).expect("placeholder .git");
    fs::write(root.join("inner/file"), b"data\n").expect("write file");

    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add all");

    let index = repo.index_handle().load().expect("index");
    assert!(
        index.get(b"inner", 0).is_none(),
        "placeholder inner/.git must not create a gitlink at inner"
    );
    assert!(
        index.get(b"inner/file", 0).is_some(),
        "inner/file must be staged as a normal blob"
    );

    let grit_ls = git(root, &["ls-files", "-s"]);
    assert!(
        grit_ls.contains("inner/file"),
        "git ls-files must list inner/file: {grit_ls}"
    );
    assert!(
        !grit_ls.contains("160000"),
        "git must not record a gitlink: {grit_ls}"
    );
}

#[test]
fn add_stages_nested_linked_worktree_as_gitlink() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.email", "t@example.com"]);
    git(root, &["config", "user.name", "Test"]);
    fs::write(root.join("a"), b"a\n").expect("write a");
    git(root, &["add", "a"]);
    git(root, &["commit", "-qm", "seed"]);

    let nested_home = tmp.path().join("nested-home");
    fs::create_dir_all(&nested_home).expect("mkdir nested-home");
    git(&nested_home, &["init", "-q", "-b", "main"]);
    git(&nested_home, &["config", "user.email", "t@example.com"]);
    git(&nested_home, &["config", "user.name", "Test"]);
    fs::write(nested_home.join("x"), b"x\n").expect("write x");
    git(&nested_home, &["add", "x"]);
    git(&nested_home, &["commit", "-qm", "x"]);

    let inner = root.join("inner");
    git(
        &nested_home,
        &["worktree", "add", "-q", inner.to_str().expect("utf8")],
    );
    assert!(
        inner.join(".git").is_file(),
        "linked worktree must use gitfile .git"
    );

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(root),
    )
    .expect("open");
    stage(
        &repo,
        &StageOptions {
            pathspecs: Vec::new(),
            pathspec_sources: Vec::new(),
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
    )
    .expect("grit add all");

    let index = repo.index_handle().load().expect("index");
    let entry = index
        .get(b"inner", 0)
        .expect("linked nested worktree must stage as gitlink at inner");
    assert_eq!(entry.mode, MODE_GITLINK);
    assert!(
        index.get(b"inner/x", 0).is_none(),
        "must not flatten linked worktree files into outer index"
    );

    let grit_ls = git(root, &["ls-files", "-s"]);
    assert!(
        grit_ls.contains("160000") && grit_ls.contains("\tinner\n"),
        "git ls-files after grit add: {grit_ls}"
    );
}
