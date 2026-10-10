//! `RefStorageFormat` parsing and detection tests.

use std::fs;
use std::sync::{Mutex, MutexGuard};

use grit_lib::error::Error;
use std::str::FromStr;

use grit_lib::ref_storage::RefStorageFormat;
use grit_lib::repo::{init_repository, validate_repo_format};
use tempfile::TempDir;

static ENV_TEST_LOCK: Mutex<()> = Mutex::new(());

fn env_test_guard() -> MutexGuard<'static, ()> {
    ENV_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

#[test]
fn parses_files_and_reftable() {
    assert_eq!(
        RefStorageFormat::from_str("files").unwrap(),
        RefStorageFormat::Files
    );
    assert_eq!(
        RefStorageFormat::from_str("REFTABLE").unwrap(),
        RefStorageFormat::Reftable
    );
    assert_eq!(
        RefStorageFormat::parse_config_value("reftable").unwrap(),
        RefStorageFormat::Reftable
    );
}

#[test]
fn rejects_unknown() {
    let err = RefStorageFormat::from_str("not-a-backend").unwrap_err();
    assert!(matches!(err, Error::InvalidRefStorageFormat { .. }));
}

#[test]
fn payload_suffix_accepted() {
    assert_eq!(
        RefStorageFormat::parse_config_value("files:v1").unwrap(),
        RefStorageFormat::Files
    );
    assert_eq!(
        RefStorageFormat::parse_config_value("reftable:experimental").unwrap(),
        RefStorageFormat::Reftable
    );
}

#[test]
fn ignores_global_config() {
    let _guard = env_test_guard();
    let tmp = TempDir::new().unwrap();
    let global = tmp.path().join("global.gitconfig");
    fs::write(&global, "[extensions]\n\trefstorage = reftable\n").unwrap();
    let root = tmp.path().join("repo");
    init_repository(&root, false, "main", None, RefStorageFormat::Files).unwrap();
    let git_dir = root.join(".git");

    let prev_global = std::env::var("GIT_CONFIG_GLOBAL").ok();
    let prev_system = std::env::var("GIT_CONFIG_SYSTEM").ok();
    std::env::set_var("GIT_CONFIG_GLOBAL", &global);
    std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");

    assert_eq!(
        RefStorageFormat::detect(&git_dir).unwrap(),
        RefStorageFormat::Files
    );

    if let Some(v) = prev_global {
        std::env::set_var("GIT_CONFIG_GLOBAL", v);
    } else {
        std::env::remove_var("GIT_CONFIG_GLOBAL");
    }
    if let Some(v) = prev_system {
        std::env::set_var("GIT_CONFIG_SYSTEM", v);
    } else {
        std::env::remove_var("GIT_CONFIG_SYSTEM");
    }
}

#[test]
fn detect_rejects_v0_reftable_extension() {
    let tmp = TempDir::new().unwrap();
    let git_dir = tmp.path().join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 0\n[extensions]\n\trefStorage = reftable\n",
    )
    .unwrap();
    let err = RefStorageFormat::detect(&git_dir).unwrap_err();
    assert!(matches!(err, Error::Message(_)));
    assert!(validate_repo_format(&git_dir).is_err());
}

#[test]
fn detect_rejects_unsupported_repository_version() {
    let tmp = TempDir::new().unwrap();
    let git_dir = tmp.path().join(".git");
    fs::create_dir_all(&git_dir).unwrap();
    fs::write(
        git_dir.join("config"),
        "[core]\n\trepositoryformatversion = 2\n",
    )
    .unwrap();
    let err = RefStorageFormat::detect(&git_dir).unwrap_err();
    assert!(matches!(err, Error::UnsupportedRepositoryFormatVersion(2)));
}

#[test]
fn detect_reads_local_reftable_config() {
    let tmp = TempDir::new().unwrap();
    init_repository(tmp.path(), false, "main", None, RefStorageFormat::Reftable).unwrap();
    let git_dir = tmp.path().join(".git");
    assert_eq!(
        RefStorageFormat::detect(&git_dir).unwrap(),
        RefStorageFormat::Reftable
    );
}
