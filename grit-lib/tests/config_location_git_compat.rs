//! Config location and origin metadata vs system `git config`.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::{
    global_config_write_path, set_config_value, unset_config_value, ConfigLocation, ConfigSet,
};
use grit_lib::config::environment::Environment;
use grit_lib::error::{ConfigError, Error};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_config_get_origin(
    repo: &Path,
    key: &str,
    env: &[(&str, &str)],
) -> Option<(String, String, String, usize)> {
    let mut cmd = Command::new("git");
    cmd.args([
        "-C",
        repo.to_str().expect("utf8 path"),
        "config",
        "--show-origin",
        "--show-scope",
        "--get",
        key,
    ]);
    for (k, v) in env {
        cmd.env(k, v);
    }
    let out = cmd.output().expect("git config");
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8(out.stdout).expect("utf8");
    let line = text.lines().next()?.trim();
    let mut parts = line.split('\t');
    let scope = parts.next()?.to_owned();
    let file = parts.next()?.strip_prefix("file:")?.to_owned();
    let value = parts.next()?.to_owned();
    Some((scope, file, value, 1))
}

#[test]
fn config_get_with_origin_matches_git_show_origin_show_scope() {
    let tmp = TempDir::new().expect("tempdir");
    let repo_path = tmp.path().join("repo");
    fs::create_dir_all(&repo_path).expect("mkdir");
    assert!(Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(&repo_path)
        .status()
        .expect("git init")
        .success());

    let global_cfg = tmp.path().join("global.cfg");
    fs::write(&global_cfg, "[user]\n\tname = Global Name\n").expect("write global");
    let local_key = "user.email";
    fs::write(
        repo_path.join(".git/config"),
        "[user]\n\temail = local@example.com\n",
    )
    .expect("write local");

    let mut env = Environment::empty();
    env.git_config_global = Some(global_cfg.display().to_string());

    let repo = Repository::discover(Some(&repo_path), &RepositoryOptions::empty()).expect("open");
    let git_dir = repo.git_dir();

    let merged = ConfigSet::load(&env, Some(git_dir), true).expect("load merged");
    let origin = merged
        .get_with_origin(local_key)
        .expect("valid key")
        .expect("set");
    assert_eq!(origin.value.as_deref(), Some("local@example.com"));
    assert_eq!(origin.scope.to_string(), "local");

    let git_origin = git_config_get_origin(
        &repo_path,
        local_key,
        &[("GIT_CONFIG_GLOBAL", global_cfg.to_str().unwrap())],
    )
    .expect("git has key");
    assert_eq!(git_origin.0, "local");
    assert_eq!(git_origin.2, "local@example.com");

    let global_only = ConfigLocation::Global
        .load_single(&env, Some(git_dir))
        .expect("global load");
    let g_origin = global_only
        .get_with_origin("user.name")
        .expect("valid")
        .expect("global name");
    assert_eq!(g_origin.value.as_deref(), Some("Global Name"));
    assert_eq!(g_origin.scope.to_string(), "global");
}

#[test]
fn config_location_global_write_path_prefers_existing_files() {
    let tmp = TempDir::new().expect("tempdir");
    let home = tmp.path().join("home");
    fs::create_dir_all(home.join(".config/git")).expect("xdg dir");
    let xdg = home.join(".config/git/config");
    fs::write(&xdg, "[user]\n\tname = xdg\n").expect("xdg file");

    let mut env = Environment::empty();
    env.home = Some(home.as_os_str().to_os_string());
    env.xdg_config_home = Some(home.join(".config").display().to_string());

    let write_path = global_config_write_path(&env).expect("write path");
    assert_eq!(write_path, xdg);

    fs::write(home.join(".gitconfig"), "[core]\n\teditor = vim\n").expect("dot gitconfig");
    let write_path = global_config_write_path(&env).expect("write path");
    assert_eq!(write_path, home.join(".gitconfig"));
}

#[test]
fn config_set_file_location_then_git_reads_it() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("standalone.cfg");
    let env = Environment::empty();

    set_config_value(
        ConfigLocation::File(cfg_path.clone()),
        &env,
        None,
        "grit.testKey",
        "hello",
    )
    .expect("set");

    let out = Command::new("git")
        .args([
            "config",
            "-f",
            cfg_path.to_str().unwrap(),
            "--get",
            "grit.testKey",
        ])
        .output()
        .expect("git");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hello");
}

#[test]
fn config_unset_missing_key_is_typed_error() {
    let tmp = TempDir::new().expect("tempdir");
    let cfg_path = tmp.path().join("empty.cfg");
    fs::write(&cfg_path, "[section]\n").expect("write");
    let env = Environment::empty();

    let err = unset_config_value(ConfigLocation::File(cfg_path), &env, None, "missing.key")
        .expect_err("missing key");
    match err {
        Error::Config(ConfigError::KeyNotSet { key }) => assert_eq!(key, "missing.key"),
        other => panic!("expected KeyNotSet, got {other:?}"),
    }
}
