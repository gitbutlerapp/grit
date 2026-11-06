//! Typed [`RepositoryError`] and config validation (factory step 7101).

use std::fs;
use std::process::Command;

use grit_lib::config::{ConfigScope, ConfigSet};
use grit_lib::error::{ConfigError, Error};
use std::path::Path;

use crate::support::grit_file_from_content;
use grit_lib::ownership_check::{OwnershipCheck, OwnershipContext, OwnershipStatus};
use grit_lib::repo::{validate_repo_config, InitOptions, Repository, RepositoryError};
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
fn repository_error_variants() {
    let missing = TempDir::new().expect("tempdir");
    let err = Repository::discover(Some(missing.path()), &Default::default()).unwrap_err();
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::NotARepository(_))
    ));

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
    let mut env = grit_lib::config::environment::Environment::empty();
    env.cwd = repo_root.clone();
    env.git_config_nosystem = Some("true".into());
    let options = grit_lib::config::environment::RepositoryOptions::with_environment(env)
        .with_ownership_check(Arc::new(TreatAllForeign));
    let err = Repository::discover(Some(&repo_root), &options).unwrap_err();
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::DubiousOwnership { .. })
    ));

    let reinit = TempDir::new().expect("tempdir");
    Repository::init(
        reinit.path(),
        &InitOptions::new().with_object_format(grit_lib::objects::HashAlgo::Sha1),
    )
    .expect("init sha1");
    let err = Repository::init(
        reinit.path(),
        &InitOptions::new().with_object_format(grit_lib::objects::HashAlgo::Sha256),
    )
    .unwrap_err();
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::InitObjectFormatChange)
    ));
}

#[test]
fn config_get_regexp_invalid_pattern() {
    let file = grit_file_from_content(
        Path::new(".git/config"),
        "[user]\n\tname = x\n",
        ConfigScope::Local,
    );
    let mut set = ConfigSet::new();
    set.merge(&file);
    let err = set.get_regexp("(").expect_err("invalid pattern");
    assert!(matches!(err, ConfigError::InvalidPattern { .. }));
}

#[test]
fn validate_repo_config_typed() {
    assert!(validate_repo_config("[core]\n\trepositoryformatversion = 0\n").is_ok());
    let err = validate_repo_config("[core]\n\trepositoryformatversion = 2\n").unwrap_err();
    assert!(matches!(
        err,
        ConfigError::UnsupportedRepositoryFormatVersion { version: 2 }
    ));
}
