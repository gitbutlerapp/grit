//! Deep `[include]` / `[includeIf]` merge and accessor coverage for `config.rs`.

mod support;

use std::fs;

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet, LoadConfigOptions};
use grit_lib::environment::Environment;
use grit_lib::repo::init_repository;
use support::{default_load_opts, isolated_env};
use tempfile::tempdir;

#[test]
fn nested_includes_three_levels() {
    let dir = tempdir().expect("tempdir");
    let root = dir.path();
    fs::write(root.join("a.conf"), "[include]\n\tpath = b.conf\n").expect("a");
    fs::write(root.join("b.conf"), "[include]\n\tpath = c.conf\n").expect("b");
    fs::write(root.join("c.conf"), "[leaf]\n\tk = deep\n").expect("c");

    let repo = root.join("repo");
    init_repository(&repo, false, "main", None, "files").expect("init");
    let git_dir = repo.join(".git");
    fs::write(
        git_dir.join("config"),
        format!("[include]\n\tpath = {}\n", root.join("a.conf").display()),
    )
    .expect("cfg");

    let env = Environment::empty();
    let set =
        ConfigSet::load_with_options(&env, Some(&git_dir), &default_load_opts(&git_dir, &env))
            .expect("load");
    assert_eq!(set.get("leaf.k").as_deref(), Some("deep"));
}

#[test]
fn compression_and_pack_accessor_matrix() {
    let snippets = [
        "[core]\n\tcompression = -1\n",
        "[core]\n\tloosecompression = 2\n",
        "[pack]\n\tcompression = 1\n",
        "[pack]\n\tthreads = 0\n",
        "[pack]\n\twriteReverseIndex = false\n",
        "[fetch]\n\twriteCommitGraph = true\n",
    ];
    for snippet in snippets {
        let file = ConfigFile::parse(std::path::Path::new("c"), snippet, ConfigScope::Local)
            .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        let _ = set.loose_objects_zlib_level();
        let _ = set.pack_objects_zlib_level();
        let _ = set.pack_index_threads();
        let _ = set.pack_write_reverse_index_default(&Environment::empty());
        let _ = set.pack_read_reverse_index_default();
        let _ = set.get_regexp("core");
    }
}

#[test]
fn load_with_env_pairs_and_global_override() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(home.join(".gitconfig"), "[fromglobal]\n\tk = g\n").expect("g");
    let repo = dir.path().join("repo");
    init_repository(&repo, false, "main", None, "files").expect("init");
    let git_dir = repo.join(".git");

    let mut env = isolated_env(&home);
    env.git_config_global = Some(home.join(".gitconfig").display().to_string());
    env.git_config_count = Some("1".into());
    env.git_config_pairs = vec![("fromenv.k".into(), "e".into())];
    env.cwd = repo.clone();

    let set = ConfigSet::load_with_options(
        &env,
        Some(&git_dir),
        &LoadConfigOptions {
            include_system: false,
            ..default_load_opts(&git_dir, &env)
        },
    )
    .expect("load");
    assert_eq!(set.get("fromenv.k").as_deref(), Some("e"));
    assert_eq!(set.get("fromglobal.k").as_deref(), Some("g"));
}
