//! Config parse failures during repository discovery (issue #2519).

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::Path;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::{InitOptions, Repository, RepositoryLocation};
use tempfile::TempDir;

fn isolated_options(cwd: &Path) -> RepositoryOptions {
    let env = Environment::from_vars(
        [
            ("GIT_CONFIG_NOSYSTEM", "1"),
            ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        cwd.to_path_buf(),
    );
    RepositoryOptions::with_environment(env)
}

fn assert_config_parse_error(err: Error) {
    match err {
        Error::Config(
            ConfigError::BadConfigLine { .. } | ConfigError::BadConfigLineInFile { .. },
        ) => {}
        other => panic!("expected config parse error, got {other:?}"),
    }
}

fn append_malformed_config(repo_root: &Path, suffix: &[u8]) {
    let config = repo_root.join(".git/config");
    let mut file = fs::OpenOptions::new()
        .append(true)
        .open(&config)
        .expect("open config");
    file.write_all(suffix).expect("append");
}

#[test]
fn discover_reports_bad_config_line_in_local_config() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    append_malformed_config(td.path(), b"[core\n");

    let err = Repository::discover(Some(td.path()), &isolated_options(td.path())).unwrap_err();
    assert_config_parse_error(err);
}

#[test]
fn discover_reports_unclosed_section_header() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    append_malformed_config(td.path(), b"[remote \"origin\n");

    let err = Repository::discover(Some(td.path()), &isolated_options(td.path())).unwrap_err();
    assert_config_parse_error(err);
}

#[test]
fn discover_reports_keyless_config_line() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    append_malformed_config(td.path(), b"\t=value\n");

    let err = Repository::discover(Some(td.path()), &isolated_options(td.path())).unwrap_err();
    assert_config_parse_error(err);
}

#[test]
fn discover_reports_invalid_core_bare_boolean() {
    let td = TempDir::new().expect("tempdir");
    Repository::init(td.path(), &InitOptions::default()).expect("init");
    fs::write(
        td.path().join(".git/config"),
        "[core]\n\trepositoryformatversion = 0\n\tbare = maybe\n",
    )
    .expect("write config");

    let err = Repository::discover(Some(td.path()), &isolated_options(td.path())).unwrap_err();
    match err {
        Error::Config(ConfigError::InvalidValue { key, .. }) => {
            assert_eq!(key, "core.bare");
        }
        other => panic!("expected invalid core.bare, got {other:?}"),
    }
}

#[test]
fn discover_reports_broken_global_gitconfig() {
    let home = TempDir::new().expect("home");
    fs::write(home.path().join(".gitconfig"), "[user\n\tname = x\n").expect("global config");

    let repo = TempDir::new().expect("repo");
    Repository::init(repo.path(), &InitOptions::default()).expect("init");

    let env = Environment::from_vars(
        [
            ("HOME", home.path().to_str().expect("home path")),
            ("GIT_CONFIG_NOSYSTEM", "1"),
        ]
        .map(|(k, v)| (OsString::from(k), OsString::from(v))),
        repo.path().to_path_buf(),
    );
    let options = RepositoryOptions::with_environment(env);

    let err = Repository::discover(Some(repo.path()), &options).unwrap_err();
    assert_config_parse_error(err);
}

#[test]
fn empty_dot_git_directory_still_skips_to_parent_repo() {
    let outer = TempDir::new().expect("outer");
    Repository::init(outer.path(), &InitOptions::default()).expect("init outer");
    let inner = outer.path().join("nested");
    fs::create_dir_all(&inner).expect("inner dir");
    fs::create_dir(inner.join(".git")).expect("empty .git");

    let options = isolated_options(&inner);
    Repository::discover(Some(&inner), &options).expect("discover parent through empty .git");
    let repo = Repository::discover(Some(&inner), &options).expect("discover parent");
    let _location = RepositoryLocation::from(&repo);
}
