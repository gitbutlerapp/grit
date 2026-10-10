//! Oracle helpers: compare grit config APIs to system `git config`.

#![allow(clippy::unwrap_used, clippy::expect_used, dead_code)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use grit_lib::config::{
    canonical_key, parse_bool, parse_color, parse_git_config_int_strict, parse_i64, parse_path,
    parse_path_optional, ConfigFile, ConfigScope, ConfigSet, IncludeContext, LoadConfigOptions,
};
use grit_lib::environment::Environment;
use grit_lib::error::{ConfigError, Error};

fn trim_git_config_stdout(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    s.strip_suffix('\n').unwrap_or(&s).to_owned()
}

pub fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn base_git_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device());
    cmd
}

pub fn git_config(args: &[&str]) -> Output {
    base_git_command()
        .args(args)
        .output()
        .expect("spawn git config")
}

pub fn git_config_with_home(home: &Path, args: &[&str]) -> Output {
    base_git_command()
        .env("HOME", home)
        .args(args)
        .output()
        .expect("spawn git config")
}

pub fn git_config_in(dir: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn git config")
}

/// `git config --file PATH --get KEY` (empty stdout + exit 1 means unset).
pub fn git_file_get(file: &Path, key: &str) -> Option<String> {
    git_file_get_includes(file, key, None)
}

/// `git config --file PATH --includes --get KEY`, optionally with `GIT_DIR` for includeIf.
pub fn git_file_get_includes(file: &Path, key: &str, git_dir: Option<&Path>) -> Option<String> {
    git_file_get_includes_with_home(file, key, git_dir, None)
}

pub fn git_file_get_includes_with_home(
    file: &Path,
    key: &str,
    git_dir: Option<&Path>,
    home: Option<&Path>,
) -> Option<String> {
    let file_arg = file.display().to_string();
    let mut cmd = base_git_command();
    cmd.args([
        "config",
        "--file",
        file_arg.as_str(),
        "--includes",
        "--get",
        key,
    ]);
    if let Some(git_dir) = git_dir {
        cmd.env("GIT_DIR", git_dir);
    }
    if let Some(home) = home {
        cmd.env("HOME", home);
    }
    let out = cmd.output().expect("spawn git config");
    if out.status.success() {
        Some(trim_git_config_stdout(&out.stdout))
    } else {
        None
    }
}

pub fn git_file_get_all(file: &Path, key: &str) -> Vec<String> {
    let file_arg = file.display().to_string();
    let out = git_config(&[
        "config",
        "--file",
        file_arg.as_str(),
        "--null",
        "--get-all",
        key,
    ]);
    if !out.status.success() {
        return Vec::new();
    }
    parse_git_null_separated_values(&out.stdout)
}

fn parse_git_null_separated_values(out: &[u8]) -> Vec<String> {
    if out.is_empty() {
        return Vec::new();
    }
    let mut vals: Vec<String> = out
        .split(|&b| b == 0)
        .map(|chunk| String::from_utf8_lossy(chunk).into_owned())
        .collect();
    if vals.last().is_some_and(String::is_empty) && vals.len() > 1 {
        vals.pop();
    }
    vals
}

pub fn git_file_get_typed(file: &Path, key: &str, ty: &str) -> Result<Option<String>, String> {
    git_file_get_typed_with_home(file, key, ty, None)
}

pub fn git_file_get_typed_with_home(
    file: &Path,
    key: &str,
    ty: &str,
    home: Option<&Path>,
) -> Result<Option<String>, String> {
    let file_arg = file.display().to_string();
    let args = [
        "config",
        "--file",
        file_arg.as_str(),
        "--type",
        ty,
        "--get",
        key,
    ];
    let out = if let Some(home) = home {
        git_config_with_home(home, &args)
    } else {
        git_config(&args)
    };
    if out.status.success() {
        return Ok(Some(trim_git_config_stdout(&out.stdout)));
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    if stderr.contains("not found") || out.status.code() == Some(1) {
        return Ok(None);
    }
    Err(format!(
        "git config --type={ty} --get {key} failed: {}",
        stderr.trim()
    ))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GitConfigLine {
    pub scope: String,
    pub origin: String,
    pub key: String,
    pub value: String,
}

/// Parse `git config --list --show-origin --show-scope [--includes]`.
pub fn git_file_list(file: &Path, includes: bool) -> Result<Vec<GitConfigLine>, String> {
    git_file_list_with_git_dir(None, file, includes)
}

/// Same as [`git_file_list`], but sets `GIT_DIR` so `includeIf.onbranch:` and similar match Grit's repo context.
pub fn git_file_list_with_git_dir(
    git_dir: Option<&Path>,
    file: &Path,
    includes: bool,
) -> Result<Vec<GitConfigLine>, String> {
    let file_arg = file.display().to_string();
    let mut args = vec![
        "config",
        "--file",
        file_arg.as_str(),
        "--list",
        "--show-origin",
        "--show-scope",
    ];
    if includes {
        args.push("--includes");
    }
    let mut cmd = base_git_command();
    cmd.args(&args);
    if let Some(git_dir) = git_dir {
        cmd.env("GIT_DIR", git_dir);
    }
    let out = cmd.output().expect("spawn git config");
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(parse_git_list_output(&String::from_utf8_lossy(&out.stdout)))
}

/// List repository-local config with `git_dir` supplying repository context.
pub fn git_local_list_with_git_dir(
    git_dir: &Path,
    includes: bool,
) -> Result<Vec<GitConfigLine>, String> {
    let mut args = vec![
        "config",
        "--local",
        "--list",
        "--show-origin",
        "--show-scope",
    ];
    if includes {
        args.push("--includes");
    }
    let out = base_git_command()
        .args(&args)
        .env("GIT_DIR", git_dir)
        .output()
        .expect("spawn git config");
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(parse_git_list_output(&String::from_utf8_lossy(&out.stdout)))
}

/// One row of the normalized config corpus: `(scope, origin, key, value)`.
pub type ConfigCorpusRow = (String, String, String, String);

fn normalize_list_scope(scope: &str) -> String {
    // `git config --file` reports `command`; Grit records standalone files as `local`.
    if scope == "command" {
        "local".to_owned()
    } else {
        scope.to_owned()
    }
}

fn normalize_list_origin(origin: &str) -> String {
    let Some(path_str) = origin.strip_prefix("file:") else {
        return origin.to_owned();
    };
    let path = Path::new(path_str);
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    format!("file:{}", canon.display())
}

fn normalize_list_value(key: &str, value: &str) -> String {
    if value.is_empty() && key.contains('.') && !key.ends_with(".path") {
        "true".to_owned()
    } else {
        value.to_owned()
    }
}

/// Normalize Git/Grit `--show-origin` list rows for ordered corpus comparison.
pub fn normalize_config_corpus(lines: &[GitConfigLine]) -> Vec<ConfigCorpusRow> {
    lines
        .iter()
        .map(|line| {
            (
                normalize_list_scope(&line.scope),
                normalize_list_origin(&line.origin),
                line.key.clone(),
                normalize_list_value(&line.key, &line.value),
            )
        })
        .collect()
}

fn parse_git_list_output(text: &str) -> Vec<GitConfigLine> {
    let mut lines = Vec::new();
    for line in text.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        let (scope, rest) = line.split_once('\t').unwrap_or((line, ""));
        let (origin, kv) = rest.split_once('\t').unwrap_or((rest, ""));
        let (key, value) = kv.split_once('=').unwrap_or((kv, ""));
        lines.push(GitConfigLine {
            scope: scope.to_owned(),
            origin: origin.to_owned(),
            key: key.to_owned(),
            value: value.to_owned(),
        });
    }
    lines
}

pub fn grit_file_from_content(path: &Path, content: &str, scope: ConfigScope) -> ConfigFile {
    ConfigFile::parse(path, content, scope).expect("grit parse config")
}

pub fn grit_get(file: &ConfigFile, key: &str) -> Option<String> {
    file.get(key)
}

pub fn grit_get_all(file: &ConfigFile, key: &str) -> Vec<String> {
    let mut set = ConfigSet::new();
    set.merge(file);
    set.get_all(key)
}

pub fn grit_get_all_raw(file: &ConfigFile, key: &str) -> Vec<Option<String>> {
    let mut set = ConfigSet::new();
    set.merge(file);
    set.get_all_raw(key)
}

pub fn isolated_env(home: &Path) -> Environment {
    let mut env = Environment::empty();
    env.home = Some(home.as_os_str().to_os_string());
    env.git_config_nosystem = Some("true".into());
    env.git_config_system = Some(null_device().into());
    env.git_config_global = Some(null_device().into());
    env
}

pub fn env_with_global(home: &Path, global_path: &Path) -> Environment {
    let mut env = isolated_env(home);
    env.git_config_global = Some(global_path.display().to_string());
    env
}

pub fn load_grit_cascade(
    env: &Environment,
    git_dir: Option<&Path>,
    opts: &LoadConfigOptions,
) -> ConfigSet {
    ConfigSet::load_with_options(env, git_dir, opts).expect("grit load config")
}

pub fn grit_list_lines(set: &ConfigSet) -> Vec<GitConfigLine> {
    set.entries()
        .iter()
        .map(|e| {
            let scope = e.scope.to_string();
            let origin = e
                .file
                .as_ref()
                .map(|p| format!("file:{}", p.display()))
                .unwrap_or_default();
            let value = e.value.clone().unwrap_or_else(|| "true".to_owned());
            GitConfigLine {
                scope,
                origin,
                key: e.key.clone(),
                value,
            }
        })
        .collect()
}

pub fn assert_file_get_matches_git(file: &Path, content: &str, key: &str) {
    std::fs::write(file, content).expect("write config");
    let git_val = git_file_get(file, key);
    let grit_file = grit_file_from_content(file, content, ConfigScope::Local);
    let grit_val = grit_get(&grit_file, key);
    match (git_val.as_deref(), grit_val.as_deref()) {
        (Some(""), Some("true")) | (Some(""), Some("")) => {}
        (None, None) => {}
        _ => assert_eq!(grit_val, git_val, "key {key} file {}", file.display()),
    }
}

pub fn assert_typed_bool_matches(file: &Path, content: &str, key: &str) {
    std::fs::write(file, content).expect("write");
    let git_val = git_file_get_typed(file, key, "bool").expect("git bool");
    let grit_file = grit_file_from_content(file, content, ConfigScope::Local);
    let grit_raw = grit_get(&grit_file, key);
    let grit_val = grit_raw.map(|v| parse_bool(&v).expect("grit bool"));
    if git_val.is_none() {
        assert!(grit_val.is_none());
        return;
    }
    assert_eq!(grit_val, Some(git_val.unwrap().parse().unwrap()));
}

pub fn assert_typed_int_matches(file: &Path, content: &str, key: &str) {
    std::fs::write(file, content).expect("write");
    let git_val = git_file_get_typed(file, key, "int").expect("git int");
    let grit_file = grit_file_from_content(file, content, ConfigScope::Local);
    let grit_raw = grit_get(&grit_file, key);
    let grit_val = grit_raw.map(|v| parse_i64(&v).expect("grit int"));
    if git_val.is_none() {
        assert!(grit_val.is_none());
        return;
    }
    assert_eq!(
        grit_val,
        Some(git_val.unwrap().parse::<i64>().expect("git int parse"))
    );
}

pub fn assert_bad_line_error(content: &str, expected_line: usize) {
    let path = Path::new("/tmp/grit-config-test/bad-line.cfg");
    let err = ConfigFile::parse(path, content, ConfigScope::Local).unwrap_err();
    match err {
        Error::Config(ConfigError::BadConfigLine { line, .. })
        | Error::Config(ConfigError::BadConfigLineInFile { line, .. }) => {
            assert_eq!(
                line, expected_line,
                "bad line number for content:\n{content}"
            );
        }
        other => panic!("expected BadConfigLine, got {other:?}"),
    }
}

pub fn assert_canonical_key_err(raw: &str) {
    assert!(
        canonical_key(raw).is_err(),
        "expected canonical_key error for {raw:?}"
    );
}

pub fn git_get_urlmatch(file: &Path, variable: &str, url: &str) -> Result<Vec<String>, String> {
    let file_arg = file.display().to_string();
    let out = git_config(&[
        "config",
        "--file",
        file_arg.as_str(),
        "--get-urlmatch",
        variable,
        url,
    ]);
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

pub fn write_layered_repo(home: &Path, global: &str, local: &str) -> PathBuf {
    std::fs::create_dir_all(home).expect("home");
    std::fs::write(home.join(".gitconfig"), global).expect("global");
    let repo = home.join("repo");
    let init = Command::new("git")
        .args([
            "init",
            "-q",
            "-b",
            "main",
            repo.display().to_string().as_str(),
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .env("HOME", home)
        .status()
        .expect("git init");
    assert!(init.success(), "git init failed");
    std::fs::write(repo.join(".git/config"), local).expect("local");
    repo
}

pub fn default_load_opts(git_dir: &Path, env: &Environment) -> LoadConfigOptions {
    LoadConfigOptions {
        include_system: false,
        process_includes: true,
        command_includes: true,
        include_ctx: IncludeContext {
            git_dir: Some(git_dir.to_path_buf()),
            cwd: env.cwd.clone(),
            pwd: env.pwd.clone(),
            env: std::sync::Arc::new(env.clone()),
            ..Default::default()
        },
        ..Default::default()
    }
}

pub fn grit_path_value(env: &Environment, raw: &str) -> String {
    parse_path(env, raw)
}

pub fn grit_path_optional(env: &Environment, raw: &str) -> Option<String> {
    parse_path_optional(env, raw)
}

pub fn git_path_value(file: &Path, key: &str) -> Option<String> {
    git_file_get_typed(file, key, "path").ok().flatten()
}

pub fn grit_color(raw: &str) -> Result<String, String> {
    parse_color(raw)
}

pub fn git_color(file: &Path, key: &str) -> Option<String> {
    git_file_get_typed(file, key, "color").ok().flatten()
}

pub fn grit_strict_int(raw: &str) -> Result<i64, grit_lib::config::GitConfigIntStrictError> {
    parse_git_config_int_strict(raw)
}
