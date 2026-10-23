//! Config location and origin metadata vs system `git config`.

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::config::environment::{Environment, RepositoryOptions};
use grit_lib::config::{
    global_config_write_path, set_config_value, unset_config_value, ConfigLocation, ConfigScope,
    ConfigSet,
};
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::Repository;
use tempfile::TempDir;

fn git_config_get_origin(
    repo: &Path,
    key: &str,
    env: &[(&str, &str)],
) -> Option<(String, String, String)> {
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
    let origin = parts.next()?.to_owned();
    let value = parts.next()?.to_owned();
    let file = if scope == "command" {
        String::new()
    } else {
        origin.strip_prefix("file:")?.to_owned()
    };
    Some((scope, file, value))
}

fn git_config_file_get_origin(cfg: &Path, key: &str) -> Option<(String, String, String)> {
    let out = Command::new("git")
        .args([
            "config",
            "-f",
            cfg.to_str().unwrap(),
            "--show-origin",
            "--show-scope",
            "--get",
            key,
        ])
        .output()
        .expect("git config");
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8(out.stdout).expect("utf8");
    let line = line.lines().next()?.trim();
    let mut parts = line.split('\t');
    let scope = parts.next()?.to_owned();
    let file = parts.next()?.strip_prefix("file:")?.to_owned();
    let value = parts.next()?.to_owned();
    Some((scope, file, value))
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

    let system_cfg = tmp.path().join("system.cfg");
    fs::write(&system_cfg, "[user]\n\tname = System User\n").expect("write system");
    let global_cfg = tmp.path().join("global.cfg");
    fs::write(&global_cfg, "[user]\n\tname = Global Name\n").expect("write global");
    fs::write(
        repo_path.join(".git/config"),
        "[user]\n\temail = local@example.com\n",
    )
    .expect("write local");

    let mut env = Environment::empty();
    env.git_config_system = Some(system_cfg.display().to_string());
    env.git_config_global = Some(global_cfg.display().to_string());
    env.git_config_parameters = Some("'user.name=Command Name'".to_owned());

    let repo = Repository::discover(Some(&repo_path), &RepositoryOptions::empty()).expect("open");
    let git_dir = repo.git_dir();

    let merged = ConfigSet::load(&env, Some(git_dir), true).expect("load merged");
    let origin = merged
        .get_with_origin("user.name")
        .expect("valid key")
        .expect("set");
    assert_eq!(origin.value.as_deref(), Some("Command Name"));
    assert_eq!(origin.scope, ConfigScope::Command);
    assert!(origin.file.is_none());
    assert!(origin.line.is_none());

    let git_origin = git_config_get_origin(
        &repo_path,
        "user.name",
        &[
            ("GIT_CONFIG_SYSTEM", system_cfg.to_str().unwrap()),
            ("GIT_CONFIG_GLOBAL", global_cfg.to_str().unwrap()),
            ("GIT_CONFIG_PARAMETERS", "'user.name=Command Name'"),
        ],
    )
    .expect("git has key");
    assert_eq!(git_origin.0, "command");
    assert_eq!(git_origin.2, "Command Name");

    let email = merged
        .get_with_origin("user.email")
        .expect("valid")
        .expect("local email");
    assert_eq!(email.scope, ConfigScope::Local);
    assert_eq!(email.value.as_deref(), Some("local@example.com"));

    let global_only = ConfigLocation::Global
        .load_single(&env, Some(git_dir))
        .expect("global load");
    let g_origin = global_only
        .get_with_origin("user.name")
        .expect("valid")
        .expect("global name");
    assert_eq!(g_origin.value.as_deref(), Some("Global Name"));
    assert_eq!(g_origin.scope, ConfigScope::Global);

    let system_only = ConfigLocation::System
        .load_single(&env, None)
        .expect("system load");
    let s_origin = system_only
        .get_with_origin("user.name")
        .expect("valid")
        .expect("system name");
    assert_eq!(s_origin.value.as_deref(), Some("System User"));
    assert_eq!(s_origin.scope, ConfigScope::System);

    let file_cfg = tmp.path().join("file.cfg");
    fs::write(&file_cfg, "[demo]\n\tvalue = from-file\n").expect("write file cfg");
    let file_set = ConfigLocation::File(file_cfg.clone())
        .load_single(&Environment::empty(), None)
        .expect("file load");
    let f_origin = file_set
        .get_with_origin("demo.value")
        .expect("valid")
        .expect("file value");
    assert_eq!(f_origin.scope, ConfigScope::Command);
    assert_eq!(f_origin.file.as_deref(), Some(file_cfg.as_path()));
    assert!(f_origin.line.is_some());
    let git_file = git_config_file_get_origin(&file_cfg, "demo.value").expect("git file get");
    assert_eq!(git_file.0, "command");
    assert_eq!(git_file.1, file_cfg.display().to_string());
    assert_eq!(git_file.2, "from-file");
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
