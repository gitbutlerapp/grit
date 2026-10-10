//! Additional `[includeIf]` and include-validation paths for config line coverage.

mod support;

use std::fs;

use grit_lib::config::{ConfigScope, ConfigSet, IncludeContext};
use grit_lib::environment::Environment;
use grit_lib::error::{ConfigError, Error};
use grit_lib::repo::init_repository;
use support::{grit_file_from_content, isolated_env};
use tempfile::tempdir;

fn ctx(git_dir: &std::path::Path, env: &Environment) -> IncludeContext {
    IncludeContext {
        git_dir: Some(git_dir.to_path_buf()),
        cwd: env.cwd.clone(),
        pwd: env.pwd.clone(),
        env: std::sync::Arc::new(env.clone()),
        ..Default::default()
    }
}

#[test]
fn includeif_onbranch_matches_current_branch() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "feature/x",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str("[includeIf \"onbranch:feature/x\"]\n\tpath = branch.conf\n");
    fs::write(git_dir.join("config"), &cfg).expect("write cfg");
    fs::write(git_dir.join("branch.conf"), "[hit]\n\tk = yes\n").expect("branch");

    let env = Environment::empty();
    let set = ConfigSet::load_with_options(
        &env,
        Some(&git_dir),
        &support::default_load_opts(&git_dir, &env),
    )
    .expect("load");
    assert_eq!(set.get("hit.k").as_deref(), Some("yes"));
}

#[test]
fn includeif_hasconfig_remote_url() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str(
        "[remote \"origin\"]\n\turl = https://example.com/proj.git\n\
         [includeIf \"hasconfig:remote.*.url:https://example.com/*\"]\n\tpath = remote.conf\n",
    );
    fs::write(git_dir.join("config"), &cfg).expect("write cfg");
    fs::write(git_dir.join("remote.conf"), "[fromremote]\n\tk = 1\n").expect("remote inc");

    let env = Environment::empty();
    let set = ConfigSet::load_with_options(
        &env,
        Some(&git_dir),
        &support::default_load_opts(&git_dir, &env),
    )
    .expect("load");
    assert_eq!(set.get("fromremote.k").as_deref(), Some("1"));
}

#[test]
fn hasconfig_include_rejects_remote_url_in_included_file() {
    let dir = tempdir().expect("tempdir");
    let main = dir.path().join("main.conf");
    fs::write(
        &main,
        "[includeIf \"hasconfig:remote.*.url:https://x/*\"]\n\tpath = bad.conf\n",
    )
    .expect("main");
    fs::write(
        dir.path().join("bad.conf"),
        "[remote \"r\"]\n\turl = https://evil.example/x\n",
    )
    .expect("bad");

    let content = fs::read_to_string(&main).expect("read");
    let file = grit_file_from_content(&main, &content, ConfigScope::Local);
    let mut set = ConfigSet::new();
    let env = Environment::empty();
    let err = set
        .merge_file_with_includes(&file, true, &ctx(dir.path(), &env))
        .expect_err("remote url in hasconfig include");
    assert!(matches!(
        err,
        Error::Config(ConfigError::RemoteUrlInHasconfigInclude)
    ));
}

#[test]
fn parse_rejects_bad_lines_and_negotiation_algorithm() {
    use grit_lib::config::ConfigFile;

    let err = ConfigFile::parse(
        std::path::Path::new("c"),
        "[s]\n\tfetch.negotiationalgorithm\n",
        ConfigScope::Local,
    )
    .expect_err("missing value");
    assert!(matches!(err, Error::Message(_) | Error::Config(_)));

    let err = ConfigFile::parse(
        std::path::Path::new("c"),
        "[s]\n\tkey = \"unclosed\n",
        ConfigScope::Local,
    )
    .expect_err("unclosed quote");
    assert!(matches!(
        err,
        Error::Config(ConfigError::BadConfigLineInFile { .. })
    ));

    let file = ConfigFile::parse(
        std::path::Path::new("c"),
        "[inline \"sub\"]\n\tinside = 1\n",
        ConfigScope::Local,
    )
    .expect("inline section key");
    assert_eq!(file.get("inline.sub.inside").as_deref(), Some("1"));
}

#[test]
fn includeif_gitdir_icase_matches() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let repo = home.join("Repo");
    init_repository(
        &repo,
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = repo.join(".git");
    let pattern = format!("gitdir/i:{}/", repo.display());
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str(&format!("[includeIf \"{pattern}\"]\n\tpath = icase.conf\n"));
    fs::write(git_dir.join("config"), &cfg).expect("cfg");
    fs::write(git_dir.join("icase.conf"), "[icase]\n\tk = 1\n").expect("inc");

    let mut env = isolated_env(&home);
    env.cwd = repo.clone();
    env.pwd = Some(repo.display().to_string());
    let set = ConfigSet::load_with_options(
        &env,
        Some(&git_dir),
        &support::default_load_opts(&git_dir, &env),
    )
    .expect("load");
    assert_eq!(set.get("icase.k").as_deref(), Some("1"));
}
