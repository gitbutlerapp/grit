//! ConfigFile mutation and round-trip write coverage.

mod support;

use std::fs;
use std::path::Path;

use grit_lib::config::{ConfigFile, ConfigIncludeOrigin, ConfigScope, ConfigSet};
use grit_lib::environment::Environment;
use grit_lib::error::{ConfigError, Error};
use tempfile::tempdir;

#[test]
fn set_unset_replace_and_count() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(&path, "[a]\n\tx = 1\n\tx = 2\n").expect("write");
    let mut file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    assert_eq!(file.count("a.x").expect("count"), 2);
    file.replace_all("a.x", "9", None).expect("replace");
    assert_eq!(file.get("a.x").as_deref(), Some("9"));
    assert_eq!(file.unset("a.x").expect("unset"), 1);
    assert!(file.get("a.x").is_none());
    file.add_value("a.y", "new").expect("add");
    file.write().expect("persist");
    let again = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read2")
        .expect("exists2");
    assert_eq!(again.get("a.y").as_deref(), Some("new"));
}

#[test]
fn unset_matching_and_last() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(
        &path,
        "[r]\n\turl = a\n\turl = b\n\turl = c\n[s]\n\turl = only\n",
    )
    .expect("write");
    let mut file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    assert!(matches!(
        file.unset_last("r.url"),
        Err(Error::Config(ConfigError::MultipleValues { key })) if key == "r.url"
    ));
    assert_eq!(file.count("r.url").expect("count"), 3);
    file.unset_last("s.url")
        .expect("unset last single-valued key");
    assert!(file.get("s.url").is_none());
    let n = file
        .unset_matching("r.url", None, false)
        .expect("unset all");
    assert_eq!(n, 3);
}

#[test]
fn rename_and_remove_section() {
    let dir = tempdir().expect("tempdir");
    let path = dir.path().join("config");
    fs::write(&path, "[old]\n\tk = v\n[stay]\n\tk2 = v2\n").expect("write");
    let mut file = ConfigFile::from_path(&path, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    assert!(file.rename_section("old", "new").expect("rename"));
    assert!(file.get("new.k").is_some());
    assert!(file.remove_section("stay").expect("remove"));
    file.write().expect("write");
}

#[test]
fn parse_with_origin_and_include_origin() {
    let content = "[c]\n\tk = v\n";
    let file = ConfigFile::parse_with_origin(
        Path::new("-"),
        content,
        ConfigScope::Command,
        ConfigIncludeOrigin::Stdin,
    )
    .expect("parse");
    assert_eq!(file.include_origin, ConfigIncludeOrigin::Stdin);
}

#[test]
fn config_set_pack_and_log_helpers() {
    let text = r#"
[core]
	logAllRefUpdates = always
	bare = false
[pack]
	writeReverseIndex = false
	readReverseIndex = false
	threads = 4
[core]
	compression = 3
"#;
    let file = ConfigFile::parse(Path::new("cfg"), text, ConfigScope::Local).expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&file);
    let env = Environment::empty();
    assert!(!set.pack_write_reverse_index_default(&env));
    assert!(!set.pack_read_reverse_index_default());
    assert_eq!(set.pack_index_threads(), Some(4));
    assert_eq!(set.loose_objects_zlib_level().expect("zlib"), 3);
    assert_eq!(
        set.effective_log_refs_config(Path::new(".git")),
        grit_lib::refs::LogRefsConfig::Always
    );
    assert!(!set.quote_path_fully() || set.get_bool("core.quotepath").is_none());
}

#[test]
fn resolve_diff_context_and_global_paths() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    let mut env = support::isolated_env(&home);
    env.xdg_config_home = Some(home.join(".config").display().to_string());
    let _paths = grit_lib::config::global_config_paths_pub(&env);
    let good = ConfigFile::parse(
        Path::new("c"),
        "[diff]\n\tcontext = 5\n",
        ConfigScope::Local,
    )
    .expect("parse");
    let mut set = ConfigSet::new();
    set.merge(&good);
    assert_eq!(
        grit_lib::config::resolve_diff_context_lines(&set).expect("ctx"),
        Some(5)
    );
}
