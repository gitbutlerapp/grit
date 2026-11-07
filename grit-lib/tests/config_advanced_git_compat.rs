//! Config typed reads/writes, multi-value, sections, system, and worktree scope (t1300 subset).

use std::fs;
use std::path::Path;

use grit_lib::config::environment::Environment;
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::config::{
    canonicalize_config_write_value, system_config_path, ConfigEditor, ConfigFile, ConfigLocation,
    ConfigScope, ConfigSet, ConfigValueType, ConfigWriteOptions,
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
fn bool_or_agrees_with_git_type_bool() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let spellings = [
        "true", "Yes", "on", "1", "false", "NO", "off", "0", "", "maybe", "2",
    ];
    write_file(&path, "[t]\n\tbare\n");
    for (i, value) in spellings.iter().enumerate() {
        let status = std::process::Command::new("git")
            .args(["config", "--file", path.to_str().unwrap()])
            .arg(format!("t.k{i}"))
            .arg(value)
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .status()
            .expect("git set");
        assert!(status.success(), "git config t.k{i} {value:?}");
    }
    let content = fs::read_to_string(&path).expect("read");
    let mut set = ConfigSet::new();
    set.merge(&grit_file(&path, &content));

    let keys = std::iter::once("t.bare".to_owned())
        .chain((0..spellings.len()).map(|i| format!("t.k{i}")))
        .chain(std::iter::once("t.missing".to_owned()));
    for key in keys {
        let git = git_file_get_typed_with_home(&path, &key, "bool", &home).expect("git get");
        for default in [false, true] {
            let expected = match git.as_deref() {
                Some("true") => true,
                Some("false") => false,
                Some(other) => panic!("git printed {other:?} for {key}"),
                None => default,
            };
            assert_eq!(
                support::config_bool_with_default(&set, &key, default),
                expected,
                "{key} default {default}"
            );
        }
    }
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

    let editor = ConfigEditor::open(&env, path.clone(), ConfigScope::Local);
    let opts = ConfigWriteOptions::new();
    editor.add_value("s.k", "two", &opts).expect("add");

    let git_vals = git_file_get_all(&path, "s.k");
    assert_eq!(git_vals, vec!["one".to_owned(), "two".to_owned()]);

    editor.unset_all("s.k", &opts).expect("unset all");
    assert!(git_file_get_all(&path, "s.k").is_empty());
}

#[test]
fn config_rename_and_remove_section_git_reads_back() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    write_file(&path, "[old \"x\"]\n\turl = u\n[stay]\n\tk = 1\n");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();

    let editor = ConfigEditor::open(&env, path.clone(), ConfigScope::Local);
    let opts = ConfigWriteOptions::new();
    editor
        .rename_section("old.x", "new.x", &opts)
        .expect("rename");
    let content = fs::read_to_string(&path).unwrap();
    assert!(content.contains("[new \"x\"]"));

    editor.remove_section("stay", &opts).expect("remove");
    let git_lines = git_file_list(&path);
    assert!(!git_lines.iter().any(|l| l.starts_with("stay.")));
}

fn git_worktree_get(repo: &Path, key: &str) -> (bool, String) {
    let out = std::process::Command::new("git")
        .args(["config", "--worktree", "--get", key])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stdout).trim().to_owned(),
    )
}

#[test]
fn config_worktree_read_ignored_without_extension_git_parity() {
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
    fs::write(git_dir.join("config.worktree"), "[x]\n\tk = sneaky\n")
        .expect("write sneaky worktree config");

    let (git_ok, git_val) = git_worktree_get(&wt_path, "x.k");
    assert!(!git_ok);
    assert!(git_val.is_empty());

    let mut env = Environment::empty();
    env.cwd = wt_path.clone();
    let set = ConfigLocation::Worktree
        .load_single(&env, Some(&git_dir))
        .expect("load worktree scope");
    assert_eq!(set.get("x.k").unwrap(), None);
}

#[test]
fn config_worktree_read_enabled_matches_git() {
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
    grit_lib::repo::init_worktree_config(&git_dir).expect("enable worktree config");

    let mut env = Environment::empty();
    env.cwd = wt_path.clone();
    wt_repo
        .config()
        .add_value(
            ConfigScope::Worktree,
            "wt.key",
            "from-grit",
            &ConfigWriteOptions::new(),
        )
        .expect("set worktree key");

    let (git_ok, git_val) = git_worktree_get(&wt_path, "wt.key");
    assert!(git_ok);
    assert_eq!(git_val, "from-grit");

    let set = ConfigLocation::Worktree
        .load_single(&env, Some(&git_dir))
        .expect("load");
    assert_eq!(set.get("wt.key").unwrap().as_deref(), Some("from-grit"));
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

    let err = wt_repo
        .config()
        .add_value(
            ConfigScope::Worktree,
            "x.y",
            "z",
            &ConfigWriteOptions::new(),
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
    assert_eq!(set.get("core.a").unwrap().as_deref(), Some("1"));
}

#[test]
fn config_list_prefix_filters_canonical_keys() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    let content = "[remote \"o\"]\n\turl = u\n[core]\n\tb = t\n";
    let file = grit_file(&path, content);
    let mut set = ConfigSet::new();
    set.merge(&file);
    let filtered = set.entries_under("remote.").expect("prefix");
    assert_eq!(filtered.len(), 1);
    assert_eq!(filtered[0].key, "remote.o.url");

    let core_lower = set.entries_under("core").expect("core");
    let core_upper = set.entries_under("CORE").expect("CORE");
    assert_eq!(core_lower.len(), 1);
    assert_eq!(core_upper.len(), 1);
    assert_eq!(core_lower[0].key, "core.b");
}

#[test]
fn config_unset_last_only_removes_one_multi_value() {
    let dir = tempdir().unwrap();
    let path = dir.path().join("cfg");
    write_file(&path, "[s]\n\tk = one\n");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();
    let editor = ConfigEditor::open(&env, path.clone(), ConfigScope::Local);
    let opts = ConfigWriteOptions::new();
    editor.add_value("s.k", "two", &opts).expect("add");
    editor.unset("s.k", &opts).expect("unset one");
    let vals = git_file_get_all(&path, "s.k");
    assert_eq!(vals, vec!["one".to_owned()]);
}
