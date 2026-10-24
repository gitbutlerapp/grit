//! `ConfigOverride` / `Environment` command-line overrides, cross-checked with system `git`.
//!
//! Git exports `git -c key=value` to its children as `GIT_CONFIG_PARAMETERS`; grit must write
//! the same encoding (so hooks and helpers that run `git` see the overrides) and read Git's.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{ConfigOverride, Environment, RepositoryOptions};
use grit_lib::config::ConfigSet;
use grit_lib::repo::Repository;

const SPECS: &[&str] = &[
    "user.name=it's a !bang",
    "core.bare",
    "user.email=",
    "http.extraHeader=Authorization: Bearer a=b",
    "remote.My Origin.url=/srv/x y",
];

fn hermetic_git(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir)
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("HOME", dir);
    cmd
}

fn overrides_env(dir: &Path) -> Environment {
    let mut env = Environment::empty();
    env.cwd = dir.to_path_buf();
    env.home = Some(dir.as_os_str().to_owned());
    env.git_config_nosystem = Some("1".to_owned());
    env.git_config_global = Some("/dev/null".to_owned());
    for spec in SPECS {
        env.push_config_override(&spec.parse::<ConfigOverride>().unwrap());
    }
    env
}

#[test]
fn grit_encoding_matches_what_git_exports_for_dash_c() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = hermetic_git(dir.path());
    for spec in SPECS {
        cmd.arg("-c").arg(spec);
    }
    let out = cmd
        .args([
            "-c",
            "alias.dump=!printf %s \"$GIT_CONFIG_PARAMETERS\"",
            "dump",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let from_git = String::from_utf8(out.stdout).unwrap();

    let ours = overrides_env(dir.path()).git_config_parameters.unwrap();
    let git_alias_entry = " 'alias.dump'=";
    let from_git = &from_git[..from_git.find(git_alias_entry).unwrap()];
    assert_eq!(ours, from_git);
}

#[test]
fn git_reads_grit_overrides_from_subprocess_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env = overrides_env(dir.path());
    let mut cmd = hermetic_git(dir.path());
    cmd.envs(
        env.subprocess_environment()
            .into_iter()
            .filter(|(k, _)| k == "GIT_CONFIG_PARAMETERS"),
    );
    let out = cmd
        .args(["config", "--list", "--show-scope"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let listed = String::from_utf8(out.stdout).unwrap();
    for expected in [
        "command\tuser.name=it's a !bang",
        "command\tcore.bare",
        "command\tuser.email=",
        "command\thttp.extraheader=Authorization: Bearer a=b",
        "command\tremote.My Origin.url=/srv/x y",
    ] {
        assert!(
            listed.lines().any(|l| l == expected),
            "missing {expected:?} in\n{listed}"
        );
    }
}

#[test]
fn grit_reads_overrides_git_exported() {
    let dir = tempfile::tempdir().unwrap();
    let out = hermetic_git(dir.path())
        .args(["-c", "user.name=From Git", "-c", "core.bare"])
        .args([
            "-c",
            "alias.dump=!printf %s \"$GIT_CONFIG_PARAMETERS\"",
            "dump",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();
    env.git_config_nosystem = Some("1".to_owned());
    env.git_config_global = Some("/dev/null".to_owned());
    env.git_config_parameters = Some(String::from_utf8(out.stdout).unwrap());
    env.push_config_override(&"user.name=From Grit".parse().unwrap());

    let config = ConfigSet::load(&env, None, false).unwrap();
    assert_eq!(config.get("user.name").as_deref(), Some("From Grit"));
    assert_eq!(config.get_bool("core.bare"), Some(Ok(true)));
}

#[test]
fn git_dir_and_work_tree_open_a_git_created_repository_from_elsewhere() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("r");
    let status = hermetic_git(dir.path())
        .args(["init", "-q", "-b", "main", "r"])
        .status()
        .unwrap();
    assert!(status.success());
    let other = dir.path().join("other");
    std::fs::create_dir_all(&other).unwrap();

    let mut env = Environment::empty();
    env.cwd = other.clone();
    env.git_config_nosystem = Some("1".to_owned());
    env.git_config_global = Some("/dev/null".to_owned());
    let env = env.with_git_dir("../r/.git").with_work_tree("../r");
    let repo = Repository::discover(None, &RepositoryOptions::with_environment(env)).unwrap();
    assert_eq!(
        repo.git_dir().canonicalize().unwrap(),
        repo_dir.join(".git").canonicalize().unwrap()
    );
    assert_eq!(
        repo.work_tree().unwrap().canonicalize().unwrap(),
        repo_dir.canonicalize().unwrap()
    );
}

#[test]
fn invalid_override_keys_are_typed_config_errors() {
    for spec in ["nosection=x", "=x", "user.=x"] {
        let err = spec.parse::<ConfigOverride>().unwrap_err();
        assert!(
            matches!(err, grit_lib::error::Error::Config(_)),
            "{spec}: {err:?}"
        );
    }
    let ok = ConfigOverride::new("user.name", Some("CI".to_owned())).unwrap();
    assert_eq!((ok.key(), ok.value()), ("user.name", Some("CI")));
}
