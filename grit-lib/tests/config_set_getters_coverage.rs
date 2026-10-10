//! Exercise [`ConfigSet`] typed accessors and merge paths for line coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet, LoadConfigOptions};
use grit_lib::environment::Environment;
use grit_lib::repo::init_repository;
use tempfile::tempdir;

mod support;

#[test]
fn typed_getters_and_merge_set() {
    let text = r#"
[core]
    bare = false
    logAllRefUpdates = true
    quotePath = false
    compression = 1
[pack]
    compression = 7
    threads = 2
    writeReverseIndex = true
    readReverseIndex = true
[fetch]
    writeCommitGraph = false
[remote "origin"]
    url = https://example.com/r.git
    fetch = +refs/heads/*:refs/remotes/origin/*
[branch "main"]
    remote = origin
    merge = refs/heads/main
"#;
    let f1 = ConfigFile::parse(Path::new("a"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&f1);
    let mut set2 = ConfigSet::new();
    set2.merge_set(&set);
    assert_eq!(set2.get("core.bare").as_deref(), Some("false"));
    assert_eq!(set2.get_all("remote.origin.fetch").len(), 1);
    assert!(set2.has_key("branch.main.remote"));
    assert_eq!(set2.get_bool("core.bare"), Some(Ok(false)));
    let env = Environment::empty();
    assert!(set2.pack_write_reverse_index_default(&env));
    assert!(set2.pack_read_reverse_index_default());
    assert_eq!(set2.pack_index_threads(), Some(2));
    assert_eq!(set2.loose_objects_zlib_level().unwrap(), 1);
    assert_eq!(set2.pack_objects_zlib_level().unwrap(), 7);
    let _ = set2.pack_index_parallelism();
}

#[test]
fn load_cascade_hits_local_and_global_layers() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(
        home.join(".gitconfig"),
        "[user]\n\tname = Global\n[core]\n\tautocrlf = true\n",
    )
    .expect("global");
    let repo = dir.path().join("repo");
    init_repository(
        &repo,
        false,
        "main",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = repo.join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str("[user]\n\temail = local@example.com\n");
    fs::write(git_dir.join("config"), cfg).expect("write local");

    let mut env = support::isolated_env(&home);
    env.git_config_global = Some(home.join(".gitconfig").display().to_string());
    env.cwd = repo.clone();
    let opts = LoadConfigOptions {
        include_system: false,
        ..support::default_load_opts(&git_dir, &env)
    };
    let set = ConfigSet::load_with_options(&env, Some(&git_dir), &opts).expect("load");
    assert_eq!(set.get("user.email").as_deref(), Some("local@example.com"));
    assert_eq!(set.get("user.name").as_deref(), Some("Global"));
    assert_eq!(set.get("core.autocrlf").as_deref(), Some("true"));
}

#[test]
fn config_write_lock_conflict() {
    use grit_lib::config::config_lock_path;
    use grit_lib::error::{ConfigError, Error};

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(&path, "[a]\n\tk = 1\n").expect("write");
    let file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    fs::write(config_lock_path(&path), b"locked").expect("lock");
    let err = file.write().expect_err("locked");
    assert!(matches!(
        err,
        Error::Config(ConfigError::ConfigFileLocked { .. })
    ));
}

#[test]
fn effective_log_refs_config_and_i64_errors() {
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
    cfg.push_str("[core]\n\tlogAllRefUpdates = always\n[invalid]\n\ti = not-int\n");
    fs::write(git_dir.join("config"), cfg).expect("write");
    let set = ConfigSet::load_repo_local_only(&git_dir).expect("local");
    let _ = set.effective_log_refs_config(&git_dir);
    assert!(set.get_i64("invalid.i").unwrap().is_err());
}
