//! Issue #2360: warn when nested repositories are recorded as gitlinks.

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::MODE_GITLINK;
use grit_lib::porcelain::add::{stage, StageMode, StageOptions};
use grit_lib::porcelain::status::{status, IgnoredMode, StatusOptions, UntrackedMode};
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
    fs::write(inner.join("nested.txt"), b"inside\n").expect("write");
    git(inner, &["add", "nested.txt"]);
    git(inner, &["commit", "-qm", "nested"]);
}

#[test]
fn status_lists_embedded_repository_before_staging() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    let vendor = root.join("vendor-hello");
    fs::create_dir_all(&vendor).expect("mkdir");
    init_nested_git_repo(&vendor);

    let model = status(
        &repo,
        &StatusOptions {
            untracked: UntrackedMode::Normal,
            ignored: IgnoredMode::No,
            ..StatusOptions::default()
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("status");

    assert!(
        model
            .embedded_repositories
            .iter()
            .any(|p| p.starts_with("vendor-hello")),
        "expected vendor-hello in embedded_repositories: {:?}",
        model.embedded_repositories
    );
}

#[test]
fn add_and_commit_report_new_embedded_gitlink() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    fs::write(root.join("a"), b"a\n").expect("top");
    let vendor = root.join("vendor-hello");
    fs::create_dir_all(&vendor).expect("mkdir");
    init_nested_git_repo(&vendor);

    let stage_outcome = stage(
        &repo,
        &StageOptions {
            pathspecs: vec!["vendor-hello".to_owned()],
            pathspec_sources: vec!["vendor-hello".to_owned()],
            mode: StageMode::All,
            ..StageOptions::default()
        },
        &mut NullProgress,
        &grit_lib::progress::NeverCancel,
    )
    .expect("stage");

    assert_eq!(
        stage_outcome.embedded_gitlinks,
        vec!["vendor-hello".to_owned()]
    );

    let index = repo.index_handle().load().expect("index");
    let entry = index.get(b"vendor-hello", 0).expect("gitlink indexed");
    assert_eq!(entry.mode, MODE_GITLINK);

    let ls = git(root, &["ls-files", "-s"]);
    assert!(ls.contains("160000") && ls.contains("vendor-hello"));
}
