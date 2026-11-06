//! Typed [`RepositoryError`] and config validation (factory step 7101).

use std::path::Path;

use grit_lib::config::{ConfigScope, ConfigSet};
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::{validate_repo_config, InitOptions, Repository, RepositoryError};
use tempfile::TempDir;

use crate::support::grit_file_from_content;

#[test]
fn repository_error_variants_without_git() {
    let missing = TempDir::new().expect("tempdir");
    let err = Repository::discover(Some(missing.path()), &Default::default()).unwrap_err();
    assert!(matches!(
        err,
        Error::Repository(RepositoryError::NotARepository(_))
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
