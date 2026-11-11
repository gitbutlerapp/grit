//! Issue #2360: warn when nested repositories are recorded as gitlinks.

use grit_lib::progress::NullProgress;
use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::MODE_GITLINK;
use grit_lib::porcelain::add::{StageMode, StageOptions};
use grit_lib::porcelain::status::StatusOptions;
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

    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
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

    let stage_outcome = repo
        .index()
        .stage(
            &StageOptions {
                pathspecs: vec!["vendor-hello".to_owned()],
                pathspec_sources: vec!["vendor-hello".to_owned()],
                mode: StageMode::All,
                ..StageOptions::default()
            },
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");

    assert_eq!(
        stage_outcome.embedded_gitlinks,
        vec!["vendor-hello".to_owned()]
    );

    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let entry = index.get(b"vendor-hello", 0).expect("gitlink indexed");
    assert_eq!(entry.mode, MODE_GITLINK);

    let ls = git(root, &["ls-files", "-s"]);
    assert!(ls.contains("160000") && ls.contains("vendor-hello"));
}

#[cfg(unix)]
#[test]
fn symlink_to_repo_root_is_not_embedded_repository() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    fs::write(root.join("a"), b"a\n").expect("top");
    std::os::unix::fs::symlink(".", root.join("loop")).expect("symlink");

    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("status");
    assert!(
        model.embedded_repositories.is_empty(),
        "symlink must not appear as embedded repo: {:?}",
        model.embedded_repositories
    );

    let stage_outcome = repo
        .index()
        .stage(
            &StageOptions {
                pathspecs: vec![".".to_owned()],
                pathspec_sources: vec![".".to_owned()],
                mode: StageMode::All,
                ..StageOptions::default()
            },
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");
    assert!(stage_outcome.embedded_gitlinks.is_empty());

    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let entry = index.get(b"loop", 0).expect("symlink indexed");
    assert_eq!(entry.mode, grit_lib::index::MODE_SYMLINK);

    let ls = git(root, &["ls-files", "-s"]);
    assert!(ls.contains("120000") && ls.contains("loop"));
}

#[cfg(unix)]
#[test]
fn symlink_to_other_repository_directory_is_not_embedded() {
    if Command::new("git").arg("--version").output().is_err() {
        return;
    }

    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    let outer = tempfile::tempdir().expect("outer");
    let outside = outer.path().join("repo");
    fs::create_dir_all(&outside).expect("mkdir");
    init_nested_git_repo(&outside);
    std::os::unix::fs::symlink(&outside, root.join("ext")).expect("symlink");

    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("status");
    assert!(model.embedded_repositories.is_empty());

    let stage_outcome = repo
        .index()
        .stage(
            &StageOptions {
                pathspecs: vec!["ext".to_owned()],
                pathspec_sources: vec!["ext".to_owned()],
                mode: StageMode::All,
                ..StageOptions::default()
            },
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("stage");
    assert!(stage_outcome.embedded_gitlinks.is_empty());

    let index = repo
        .index()
        .load(&grit_lib::index::IndexLoadOptions::default())
        .expect("index");
    let entry = index.get(b"ext", 0).expect("symlink indexed");
    assert_eq!(entry.mode, grit_lib::index::MODE_SYMLINK);
}

#[cfg(unix)]
#[test]
fn real_directory_with_gitdir_symlink_still_counts_as_embedded() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    let repo = init_grit_repo(root);
    let bare_git = root.join("bare.git");
    fs::create_dir_all(bare_git.join("objects")).expect("objects");
    fs::create_dir_all(bare_git.join("refs/heads")).expect("refs");
    fs::write(bare_git.join("HEAD"), "ref: refs/heads/main\n").expect("head");
    let vendor = root.join("vendor");
    fs::create_dir_all(&vendor).expect("mkdir");
    std::os::unix::fs::symlink(&bare_git, vendor.join(".git")).expect("gitdir symlink");

    let model = repo
        .status()
        .run(
            &StatusOptions::default(),
            &mut NullProgress,
            &grit_lib::progress::CancelToken::never(),
        )
        .expect("status");
    assert!(
        model
            .embedded_repositories
            .iter()
            .any(|p| p.starts_with("vendor")),
        "real dir with .git symlink should warn: {:?}",
        model.embedded_repositories
    );
}
