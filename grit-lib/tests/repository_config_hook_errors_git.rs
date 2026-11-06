//! System `git` round-trips for [`RepositoryError`] discovery (factory step 7101).

use std::path::Path;
use std::process::Command;
use std::sync::Arc;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::Error;
use grit_lib::ownership_check::{OwnershipCheck, OwnershipContext, OwnershipStatus};
use grit_lib::repo::{Repository, RepositoryError};
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
fn repository_error_variants_with_git() {
    let format_v2 = TempDir::new().expect("tempdir");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(format_v2.path())
        .status()
        .expect("git init");
    Command::new("git")
        .args(["config", "core.repositoryformatversion", "2"])
        .current_dir(format_v2.path())
        .status()
        .expect("git config");
    let err = Repository::discover(Some(format_v2.path()), &Default::default()).unwrap_err();
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::UnsupportedRepositoryFormatVersion { version: 2 })
    ));

    let foreign = TempDir::new().expect("tempdir");
    let repo_root = foreign.path().join("repo");
    Command::new("git")
        .args(["init", "-q", repo_root.to_str().expect("utf8")])
        .status()
        .expect("git init");
    Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&repo_root)
        .env_remove("GIT_CONFIG_GLOBAL")
        .env("GIT_CONFIG_NOSYSTEM", "true")
        .env("GIT_TEST_ASSUME_DIFFERENT_OWNER", "1")
        .output()
        .expect("git status");
    let mut env = Environment::empty();
    env.cwd = repo_root.clone();
    env.git_config_nosystem = Some("true".into());
    let options =
        RepositoryOptions::with_environment(env).with_ownership_check(Arc::new(TreatAllForeign));
    let err = Repository::discover(Some(&repo_root), &options).unwrap_err();
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::DubiousOwnership { .. })
    ));
}
