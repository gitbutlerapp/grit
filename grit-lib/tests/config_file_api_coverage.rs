//! [`ConfigFile`] mutation, unset/rename, and gitmodules parse coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::config::{ConfigFile, ConfigScope, ConfigSet};
use grit_lib::environment::Environment;
use grit_lib::error::{ConfigError, Error};
use tempfile::tempdir;

#[test]
fn config_file_mutators_roundtrip_on_disk() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(&path, "[sec]\n\ta = 1\n\tb = 2\n[other]\n\tx = y\n").expect("write");

    let mut file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    file.set("sec.c", "3").expect("set");
    file.add_value("sec.a", "1b").expect("add");
    file.add_value_with_comment("sec.d", "4", Some("added"))
        .expect("add c");
    file.unset_last("sec.b").expect("unset last");
    assert_eq!(file.count("sec.a").expect("count"), 2);
    file.rename_section("other", "renamed").expect("rename");
    file.remove_section("renamed").expect("remove");
    file.write().expect("write disk");

    let reloaded = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("reload")
        .expect("exists");
    assert_eq!(reloaded.get("sec.c").as_deref(), Some("3"));
    assert!(reloaded.get("other.x").is_none());
}

#[test]
fn config_file_unset_matching_and_replace_all() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("cfg");
    let mut file = ConfigFile::parse(
        &path,
        "[m]\n\tk = a\n\tk = b\n\tk = c\n",
        ConfigScope::Local,
    )
    .expect("parse");
    let n = file
        .unset_matching("m.k", Some("b"), false)
        .expect("unset match");
    assert_eq!(n, 1);
    file.replace_all("m.k", "x", None).expect("replace one");
    file.add_value("m.k", "y").expect("add second");
    let set = {
        let mut s = ConfigSet::new();
        s.merge(&file);
        s
    };
    assert_eq!(set.get_all("m.k"), vec!["x", "y"]);
}

#[test]
fn parse_gitmodules_and_parse_with_origin() {
    let content = r#"
[submodule "lib"]
    path = lib
    url = https://example.com/lib.git
"#;
    let (entries, bad_line) = ConfigFile::parse_gitmodules_best_effort(
        Path::new(".gitmodules"),
        content,
        ConfigScope::Local,
    );
    assert!(bad_line.is_none());
    assert!(entries.iter().any(|e| e.key.contains("submodule")));

    let file = ConfigFile::parse_with_origin(
        Path::new("c"),
        "[k]\n\tv = 1\n",
        ConfigScope::Local,
        grit_lib::config::ConfigIncludeOrigin::Disk,
    )
    .expect("origin parse");
    assert_eq!(file.get("k.v").as_deref(), Some("1"));
}

#[test]
fn parse_errors_for_sections_and_integers() {
    use grit_lib::config::{parse_git_config_int_strict, parse_i64};

    let err = ConfigFile::parse(
        Path::new("c"),
        "[bad section\n\tk = 1\n",
        ConfigScope::Local,
    );
    assert!(err.is_err());

    assert!(parse_i64("not").is_err());
    assert!(parse_git_config_int_strict("1bad").is_err());

    let mut set = ConfigSet::new();
    set.add_command_override("x.y", "z").expect("cmd");
    let env = Environment::empty();
    let _par = set.pack_index_parallelism();
    let _ = env;
}

#[test]
fn config_file_replace_all_pattern_and_comments() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config-pattern");
    let mut file = ConfigFile::parse(
        &path,
        "[remote \"origin\"]\n\turl = keep\n\turl = drop\n[core]\n\tfilemode = true\n",
        ConfigScope::Local,
    )
    .expect("parse");
    file.replace_all_with_comment("remote.origin.url", "new", Some("drop"), Some("cmt"))
        .expect("pattern replace");
    file.replace_all_with_comment("remote.origin.url", "z", Some("!keep"), None)
        .expect("negated");
    file.replace_all("missing.key", "added", None)
        .expect("add via replace");
    file.set_with_comment("core.bare", "true", Some("inline"))
        .expect("set c");
    assert!(file.count("remote.origin.url").expect("count") >= 1);
    file.unset_matching("core.filemode", None, true)
        .expect("unset preserve");
    file.write().expect("write temp");
}

#[test]
fn config_file_locked_write_surfaces_typed_error() {
    use grit_lib::config::config_lock_path;

    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(&path, "[a]\n\tk = 1\n").expect("write");
    fs::write(config_lock_path(&path), b"held").expect("lock");
    let file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    let err = file.write().expect_err("locked");
    assert!(matches!(
        err,
        Error::Config(ConfigError::ConfigFileLocked { .. })
    ));
}
