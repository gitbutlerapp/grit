//! Config typed reads/writes, multi-value, sections, system, and worktree scope (t1300 subset).

use std::fs;
use std::path::Path;

use grit_lib::config::environment::Environment;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{
    add_config_value, canonicalize_config_write_value, remove_config_section,
    rename_config_section, system_config_path, unset_all_config_values, ConfigFile, ConfigLocation,
    ConfigScope, ConfigSet, ConfigValueType,
};
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::{InitOptions, Repository};
use tempfile::tempdir;

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git_file_get_all(file: &Path, key: &str) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args([
            "config",
            "--file",
            file.to_str().unwrap(),
            "--null",
            "--get-all",
            key,
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    if !out.status.success() {
        return Vec::new();
    }
    out.stdout
        .split(|&b| b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect()
}

fn git_file_get_typed_with_home(
    file: &Path,
    key: &str,
    ty: &str,
    home: &Path,
) -> Result<Option<String>, String> {
    let out = std::process::Command::new("git")
        .args([
            "config",
            "--file",
            file.to_str().unwrap(),
            "--type",
            ty,
            "--get",
            key,
        ])
        .env("HOME", home)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        let s = String::from_utf8_lossy(&out.stdout);
        return Ok(Some(s.strip_suffix('\n').unwrap_or(&s).to_owned()));
    }
    Ok(None)
}

fn git_file_list(file: &Path) -> Vec<String> {
    let out = std::process::Command::new("git")
        .args(["config", "--file", file.to_str().unwrap(), "--list"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_owned)
        .collect()
}

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("mkdir");
    }
    fs::write(path, content).expect("write");
}

fn grit_file(path: &Path, content: &str) -> ConfigFile {
    ConfigFile::parse(path, content, ConfigScope::Local).expect("parse")
}

#[test]
fn config_type_bool_int_path_canonicalize_like_git() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let mut env = Environment::empty();
    env.home = Some(home.as_os_str().to_os_string());

    let cases: &[(&str, ConfigValueType, &str)] = &[
        ("a.b", ConfigValueType::Bool, "Yes"),
        ("a.c", ConfigValueType::Int, "2k"),
        ("a.d", ConfigValueType::Path, "~/foo"),
    ];
    for (key, ty, input) in cases {
        let grit_out =
            canonicalize_config_write_value(key, *ty, input, &env).expect("canonicalize");
        write_file(&path, "");
        let ty_name = match ty {
            ConfigValueType::Bool => "bool",
            ConfigValueType::Int => "int",
            ConfigValueType::Path => "path",
        };
        let git_set = std::process::Command::new("git")
            .args([
                "config",
                "--file",
                path.to_str().unwrap(),
                "--type",
                ty_name,
                "--add",
                key,
                input,
            ])
            .env("HOME", &home)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()
            .expect("git set");
        assert!(git_set.status.success(), "git set failed: {:?}", git_set);
        let git_canon = git_file_get_typed_with_home(&path, key, ty_name, &home)
            .expect("git get")
            .expect("value");
        assert_eq!(grit_out, git_canon, "canonical {key}");
    }
}

#[test]
fn config_add_and_unset_all_round_trip_git_get_all() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    write_file(&path, "[s]\n\tk = one\n");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();

    add_config_value(
        ConfigLocation::File(path.clone()),
        &env,
        None,
        "s.k",
        "two",
        false,
    )
    .expect("add");

    let git_vals = git_file_get_all(&path, "s.k");
    assert_eq!(git_vals, vec!["one".to_owned(), "two".to_owned()]);

    unset_all_config_values(ConfigLocation::File(path.clone()), &env, None, "s.k", false)
        .expect("unset all");
    assert!(git_file_get_all(&path, "s.k").is_empty());
}

#[test]
fn config_rename_and_remove_section_git_reads_back() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    write_file(&path, "[old \"x\"]\n\turl = u\n[stay]\n\tk = 1\n");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();

    rename_config_section(
        ConfigLocation::File(path.clone()),
        &env,
        None,
        "old.x",
        "new.x",
        false,
    )
    .expect("rename");
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("[new \"x\"]"));

    remove_config_section(
        ConfigLocation::File(path.clone()),
        &env,
        None,
        "stay",
        false,
    )
    .expect("remove");
    let git_lines = git_file_list(&path);
    assert!(!git_lines.iter().any(|l| l.starts_with("stay.")));
}

#[test]
fn config_worktree_scope_requires_extension() {
    let dir = tempdir().unwrap();
    let repo_path = dir.path().join("repo");
    let wt_path = dir.path().join("wt");
    Repository::init(&repo_path, &InitOptions::default()).expect("init");
    std::process::Command::new("git")
        .args(["commit", "--allow-empty", "-m", "seed"])
        .current_dir(&repo_path)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("commit");
    let out = std::process::Command::new("git")
        .args(["worktree", "add", wt_path.to_str().unwrap()])
        .current_dir(&repo_path)
        .output()
        .expect("worktree add");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let wt_repo =
        Repository::discover(Some(&wt_path), &RepositoryOptions::empty()).expect("open wt");
    let git_dir = wt_repo.git_dir().to_path_buf();
    let mut env = Environment::empty();
    env.cwd = wt_path.clone();

    let err = add_config_value(
        ConfigLocation::Worktree,
        &env,
        Some(&git_dir),
        "x.y",
        "z",
        false,
    )
    .expect_err("worktree write blocked");
    assert!(matches!(
        err,
        Error::Config(ConfigError::WorktreeConfigNotEnabled)
    ));
}

#[test]
fn config_system_location_honors_env() {
    let dir = tempdir().unwrap();
    let system_path = dir.path().join("system.cfg");
    write_file(&system_path, "[core]\n\ta = 1\n");
    let mut env = Environment::empty();
    env.git_config_system = Some(system_path.display().to_string());
    env.git_config_nosystem = None;
    assert_eq!(system_config_path(&env), system_path);

    let set = ConfigLocation::System
        .load_single(&env, None)
        .expect("load system");
    assert_eq!(set.get("core.a").as_deref(), Some("1"));
}

#[test]
fn config_list_prefix_filters_canonical_keys() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    let content = "[remote \"o\"]\n\turl = u\n[core]\n\tb = t\n";
    let file = grit_file(&path, content);
    let mut set = ConfigSet::new();
    set.merge(&file);
    let filtered = set.entries_with_prefix("remote.").expect("prefix");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].key, "remote.o.url");
}
