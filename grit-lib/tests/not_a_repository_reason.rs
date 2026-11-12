//! [`NotARepositoryReason`] classification for discovery and `.git` resolution failures.

use std::fs;
use std::path::Path;

use crate::support::discover_optional;
use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::{Error, NotARepositoryReason};
use grit_lib::repo::{resolve_dot_git, resolve_git_directory_arg, Repository, RepositoryError};
use tempfile::TempDir;

fn discover(start: &Path) -> grit_lib::error::Result<Repository> {
    let mut env = Environment::empty();
    env.cwd = start.to_path_buf();
    Repository::discover(Some(start), &RepositoryOptions::with_environment(env))
}

fn reason(err: Error) -> NotARepositoryReason {
    match err {
        Error::Repository(RepositoryError::NotARepository(reason)) => reason,
        other => panic!("expected NotARepository, got {other:?}"),
    }
}

#[test]
fn gitfile_without_gitdir_line_is_missing_gitdir() {
    let td = TempDir::new().expect("tempdir");
    // Discovery reports canonical paths (`/var` is `/private/var` on macOS).
    let root = td.path().canonicalize().expect("canonical tempdir");
    let dot_git = root.join(".git");
    fs::write(&dot_git, "not a gitdir line\n").expect("write");

    let err = discover(&root).expect_err("invalid gitfile");
    assert_eq!(
        err.to_string(),
        "not a git repository (or any of the parent directories): invalid gitfile format"
    );
    match reason(err) {
        NotARepositoryReason::GitfileMissingGitdir { path } => assert_eq!(path, dot_git),
        other => panic!("unexpected reason {other:?}"),
    }
    assert!(matches!(
        reason(resolve_dot_git(&dot_git).expect_err("resolve")),
        NotARepositoryReason::GitfileMissingGitdir { .. }
    ));
    assert!(matches!(
        reason(resolve_git_directory_arg(&dot_git).expect_err("resolve")),
        NotARepositoryReason::GitfileMissingGitdir { .. }
    ));
}

#[test]
fn gitfile_with_missing_target_names_the_target() {
    let td = TempDir::new().expect("tempdir");
    let dot_git = td.path().join(".git");
    fs::write(&dot_git, "gitdir: elsewhere\n").expect("write");

    match reason(resolve_dot_git(&dot_git).expect_err("missing target")) {
        NotARepositoryReason::NoRepositoryAt { path } => {
            assert_eq!(path, td.path().join("elsewhere"));
        }
        other => panic!("unexpected reason {other:?}"),
    }
}

#[test]
fn gitfile_resolves_relative_and_absolute_targets() {
    let td = TempDir::new().expect("tempdir");
    let target = td.path().join("real.git");
    fs::create_dir(&target).expect("mkdir");
    let dot_git = td.path().join(".git");

    fs::write(&dot_git, "gitdir: real.git\n").expect("write");
    assert_eq!(resolve_dot_git(&dot_git).expect("relative"), target);

    fs::write(&dot_git, format!("gitdir: {}\n", target.display())).expect("write");
    assert_eq!(
        resolve_git_directory_arg(&dot_git).expect("absolute"),
        target
    );
}

#[test]
fn unreadable_gitfile_keeps_the_io_error() {
    let td = TempDir::new().expect("tempdir");
    let dot_git = td.path().join(".git");
    fs::write(&dot_git, [0xff, 0xfe, b'\n']).expect("write");

    match reason(resolve_dot_git(&dot_git).expect_err("non-UTF-8 gitfile")) {
        NotARepositoryReason::GitfileUnreadable { path, error } => {
            assert_eq!(path, dot_git);
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        }
        other => panic!("unexpected reason {other:?}"),
    }
}

#[test]
fn missing_dot_git_is_no_repository_at_that_path() {
    let td = TempDir::new().expect("tempdir");
    let dot_git = td.path().join(".git");

    match reason(resolve_dot_git(&dot_git).expect_err("missing")) {
        NotARepositoryReason::NoRepositoryAt { path } => assert_eq!(path, dot_git),
        other => panic!("unexpected reason {other:?}"),
    }
}

#[test]
fn discover_optional_reports_only_malformed_gitfiles() {
    let td = TempDir::new().expect("tempdir");
    let sub = td.path().join("sub");
    fs::create_dir(&sub).expect("mkdir");

    assert!(discover_optional(Some(&sub)).expect("no repo").is_none());

    // A malformed gitfile in a parent stops discovery and is reported even from below it.
    fs::write(td.path().join(".git"), "not a gitdir line\n").expect("write");
    assert!(matches!(
        reason(discover_optional(Some(&sub)).expect_err("malformed parent gitfile")),
        NotARepositoryReason::GitfileMissingGitdir { .. }
    ));
}

#[cfg(unix)]
#[test]
fn special_file_dot_git_is_not_a_regular_file() {
    use std::os::unix::net::UnixListener;

    let td = TempDir::new().expect("tempdir");
    // Discovery reports canonical paths (`/var` is `/private/var` on macOS).
    let root = td.path().canonicalize().expect("canonical tempdir");
    let sub = root.join("sub");
    fs::create_dir(&sub).expect("mkdir");
    let dot_git = root.join(".git");
    let _socket = UnixListener::bind(&dot_git).expect("bind socket");

    let err = discover(&sub).expect_err("socket .git");
    assert!(err.to_string().ends_with("is not a regular file"), "{err}");
    match reason(err) {
        NotARepositoryReason::GitfileNotRegularFile { path } => assert_eq!(path, dot_git),
        other => panic!("unexpected reason {other:?}"),
    }
    assert!(matches!(
        reason(discover_optional(Some(&sub)).expect_err("socket parent")),
        NotARepositoryReason::GitfileNotRegularFile { .. }
    ));

    let linked = root.join("linked");
    fs::create_dir(&linked).expect("mkdir");
    std::os::unix::fs::symlink(&dot_git, linked.join(".git")).expect("symlink");
    match reason(discover(&linked).expect_err("symlink to socket")) {
        NotARepositoryReason::GitfileNotRegularFile { path } => {
            assert_eq!(path, linked.join(".git"));
        }
        other => panic!("unexpected reason {other:?}"),
    }
}
