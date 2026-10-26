//! Regression tests for ignore fast-path classification vs system Git.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::process::Command;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::index::Index;
use grit_lib::porcelain::status::{
    collect_untracked_and_ignored, status, IgnoredMode, StatusOptions,
};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_test_support::unique_tmp;

#[path = "ignore_t0008_common/mod.rs"]
mod t0008;

fn git_check_ignored(home: &std::path::Path, repo: &std::path::Path, path: &str) -> bool {
    let out = t0008::hermetic_git(home, repo, &["check-ignore", "-q", "--no-index", path]);
    out.status.success()
}

#[test]
fn backslash_escaped_literal_matches_git_check_ignore_and_status() {
    let home = unique_tmp("ignore", "esc-literal-home");
    let root = unique_tmp("ignore", "esc-literal");
    fs::create_dir_all(&home).expect("home");
    assert!(t0008::hermetic_git(&home, &root, &["init"])
        .status
        .success());
    fs::write(root.join(".gitignore"), "foo\\ bar\n").expect("gitignore");
    fs::write(root.join("foo bar"), b"x\n").expect("file with space");

    assert!(
        git_check_ignored(&home, &root, "foo bar"),
        "git must ignore escaped-space pattern"
    );

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&root),
    )
    .expect("open");
    let index = repo.index_handle().load().expect("index");
    let check = repo
        .ignore_check(&["foo bar".into()], &index)
        .expect("ignore_check");
    assert!(check[0].ignored, "grit must ignore foo bar like git");

    let model = status(
        &repo,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert!(
        !model.untracked.iter().any(|p| p == "foo bar"),
        "status must not list ignored file, got {:?}",
        model.untracked
    );
}

#[test]
fn slash_containing_prefix_glob_respects_pathname_boundaries() {
    let home = unique_tmp("ignore", "slash-glob-home");
    let root = unique_tmp("ignore", "slash-glob");
    fs::create_dir_all(&home).expect("home");
    assert!(t0008::hermetic_git(&home, &root, &["init"])
        .status
        .success());
    fs::write(root.join(".gitignore"), "foo/*\n!foo/bar\n").expect("gitignore");
    fs::create_dir_all(root.join("foo/bar")).expect("mkdir");
    fs::write(root.join("foo/bar/baz"), b"z\n").expect("baz");

    assert!(
        !git_check_ignored(&home, &root, "foo/bar/baz"),
        "git must not ignore foo/bar/baz (negated subtree)"
    );

    let git_status = Command::new("git")
        .current_dir(&root)
        .args(["status", "--porcelain", "--untracked-files=all"])
        .env("HOME", &home)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git status");
    assert!(git_status.status.success());
    let git_out = String::from_utf8_lossy(&git_status.stdout);
    assert!(
        git_out.contains("foo/bar/baz"),
        "git status should list foo/bar/baz as untracked:\n{git_out}"
    );

    let repo = Repository::open(
        &root.join(".git"),
        &RepositoryOptions::empty().with_work_tree(&root),
    )
    .expect("open");
    let mut index: Index = repo.index_handle().load().expect("index");
    let work_tree = repo.work_tree().expect("work tree");
    let (untracked, _) =
        collect_untracked_and_ignored(&repo, &mut index, work_tree, IgnoredMode::No, true, &[])
            .expect("collect untracked");
    assert!(
        untracked.iter().any(|p| p == "foo/bar/baz"),
        "grit must list foo/bar/baz like git, got {untracked:?}"
    );
}
