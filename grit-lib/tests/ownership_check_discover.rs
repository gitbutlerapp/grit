//! Ownership check compatibility with system `git` (`safe.directory`).

use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::ownership_check::{OwnershipCheck, OwnershipContext, OwnershipStatus};
use grit_lib::repo::Repository;
use std::path::Path;
use std::sync::Arc;
use tempfile::TempDir;

#[derive(Default)]
struct TreatAllForeign;

impl OwnershipCheck for TreatAllForeign {
    fn always_apply_safe_directory(&self) -> bool {
        true
    }

    fn status(
        &self,
        _path: &Path,
        _context: &OwnershipContext,
    ) -> Result<OwnershipStatus, grit_lib::ownership_check::OwnershipError> {
        Ok(OwnershipStatus::NotOwned)
    }
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
    let options =
        RepositoryOptions::with_environment(env).with_ownership_check(Arc::new(TreatAllForeign));
    let grit_err = Repository::discover(Some(&repo_root), &options).unwrap_err();
    assert!(
        matches!(
            grit_err,
            Error::Repository(RepositoryError::DubiousOwnership { .. })
        ),
        "grit should match git: {grit_err:?}"
    );
}
