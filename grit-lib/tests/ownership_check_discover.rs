//! Ownership check injection for discovery and `safe.directory`.

use std::process::Command;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::ownership_check::{ForeignOwnershipCheck, SystemOwnershipCheck};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::TempDir;

#[test]
fn ownership_check_default_allows_owned_repo() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    let mut env = Environment::empty();
    env.cwd = td.path().to_path_buf();
    let options = RepositoryOptions::with_environment(env)
        .with_ownership_check(Arc::new(SystemOwnershipCheck));
    Repository::discover(Some(td.path()), &options).expect("owned repo opens");
}

#[test]
fn ownership_check_foreign_repo_requires_safe_directory() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    let mut env = Environment::empty();
    env.cwd = td.path().to_path_buf();
    env.git_config_nosystem = Some("true".into());
    let options = RepositoryOptions::with_environment(env)
        .with_ownership_check(Arc::new(ForeignOwnershipCheck));
    let err = Repository::discover(Some(td.path()), &options).unwrap_err();
    assert!(
        matches!(err, Error::DubiousOwnership(_)),
        "expected dubious ownership, got {err:?}"
    );
}

#[test]
fn ownership_check_foreign_repo_matches_git_safe_directory_rejection() {
    let td = TempDir::new().expect("tempdir");
    let repo_root = td.path().join("repo");
    Command::new("git")
        .args(["init", "-q", repo_root.to_str().expect("utf8")])
        .output()
        .expect("git init");

    let git_err = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("GIT_CONFIG_NOSYSTEM", "true")
        .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
        .output()
        .expect("git status");
    assert!(
        !git_err.status.success(),
        "git should reject foreign-owned repo without safe.directory: {}",
        String::from_utf8_lossy(&git_err.stderr)
    );

    let mut env = Environment::empty();
    env.cwd = repo_root.clone();
    env.git_config_nosystem = Some("true".into());
    let options = RepositoryOptions::with_environment(env)
        .with_ownership_check(Arc::new(ForeignOwnershipCheck));
    let grit_err = Repository::discover(Some(&repo_root), &options).unwrap_err();
    assert!(
        matches!(grit_err, Error::DubiousOwnership(_)),
        "grit should match git: {grit_err:?}"
    );
}
