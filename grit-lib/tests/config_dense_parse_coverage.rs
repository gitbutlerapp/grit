//! Dense parse and load snippets to ratchet `config.rs` line coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_lib::config::{
    parse_bool, parse_color, parse_git_config_int_strict, parse_i64, parse_path,
    parse_path_optional, ConfigFile, ConfigScope, ConfigSet, LoadConfigOptions,
};
use grit_lib::environment::Environment;
use grit_lib::repo::init_repository;
use tempfile::tempdir;

#[test]
fn many_parse_snippets_and_typed_parsers() {
    let snippets: &[&str] = &[
        "[s]\n\tk = v\n",
        "[sec \"sub\"]\n\tk = 1\n",
        "[sec \"sub\"]\n\tinline = x\n",
        "[a]\n\tk = line1 \\\n\t\tline2\n",
        "[m]\n\tk = a\n\tk = b\n",
        "[fetch]\n\tnegotiationAlgorithm = skip\n",
        "[core]\n\tbare = true\n\tfilemode = false\n",
        "[url \"https://x/\"]\n\tinsteadOf = y:\n",
    ];
    for content in snippets {
        let file = ConfigFile::parse(std::path::Path::new("c"), content, ConfigScope::Local)
            .expect("parse snippet");
        let mut set = ConfigSet::new();
        set.merge(&file);
        assert!(set.entries().len() >= 1);
    }

    for input in ["true", "false", "yes", "no", "on", "off", "1", "0"] {
        let _ = parse_bool(input).unwrap();
    }
    for input in ["0", "1", "42", "-3", "1k", "2m", "3g"] {
        let _ = parse_i64(input).unwrap();
    }
    let _ = parse_git_config_int_strict("0").unwrap();
    let _ = parse_color("bold red").unwrap();
    let env = Environment::empty();
    let _ = parse_path(&env, "~/x");
    let _ = parse_path_optional(&env, ":(optional,create)/no/such/path");
}

#[test]
fn load_includes_system_global_and_local_layers() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    std::fs::create_dir_all(&home).expect("home");
    let system = dir.path().join("system.conf");
    std::fs::write(&system, "[sys]\n\tk = 1\n").expect("sys");
    std::fs::write(home.join(".gitconfig"), "[usr]\n\tk = 2\n").expect("usr");
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
    std::fs::write(git_dir.join("config"), "[loc]\n\tk = 3\n").expect("loc");

    let mut env = Environment::empty();
    env.home = Some(home.as_os_str().to_os_string());
    env.git_config_system = Some(system.display().to_string());
    env.git_config_global = Some(home.join(".gitconfig").display().to_string());
    env.cwd = repo.clone();

    let set = ConfigSet::load(&env, Some(&git_dir), true).expect("full load");
    assert_eq!(set.get("loc.k").as_deref(), Some("3"));
    assert!(set.get("usr.k").is_some());
    assert!(set.get("sys.k").is_some());

    let early = ConfigSet::read_early_config(&env, Some(&git_dir), "loc.k").expect("early");
    assert!(!early.is_empty());
}

#[test]
fn command_parameters_merged_when_includes_enabled() {
    let dir = tempdir().expect("tempdir");
    let inc = dir.path().join("p.conf");
    std::fs::write(&inc, "[fromparam]\n\tk = yes\n").expect("inc");
    let mut env = Environment::empty();
    env.git_config_parameters = Some(format!(
        "'include.path={}' 'top.key=override'",
        inc.display().to_string().replace('\\', "\\\\")
    ));
    env.cwd = dir.path().to_path_buf();
    let opts = LoadConfigOptions {
        include_system: false,
        process_includes: true,
        command_includes: true,
        include_ctx: grit_lib::config::IncludeContext {
            cwd: dir.path().to_path_buf(),
            env: std::sync::Arc::new(env.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let set = ConfigSet::load_with_options(&env, None, &opts).expect("load");
    assert_eq!(set.get("top.key").as_deref(), Some("override"));
    assert_eq!(set.get("fromparam.k").as_deref(), Some("yes"));
}

#[test]
fn parse_error_branches_and_from_path_missing() {
    let bad_lines: &[&str] = &[
        "[s]\n\t!!!garbage\n",
        "[s]\n\t1bad = x\n",
        "[s]\n\tkey = \"unclosed\n",
        "[bad section\n\tk = 1\n",
        "[s]\n\tfetch.negotiationalgorithm\n",
        "[s]\n\tkey = line1 \\\n\t\tline2 \\\n\t\tline3\n",
    ];
    for content in bad_lines {
        let _ = ConfigFile::parse(std::path::Path::new("c"), content, ConfigScope::Local);
    }
    assert!(ConfigFile::parse(
        std::path::Path::new("c"),
        "[s]\n\tkey = \"unclosed\n",
        ConfigScope::Local
    )
    .is_err());
    let dir = tempdir().expect("tempdir");
    let missing = dir.path().join("nope.conf");
    assert!(ConfigFile::from_path(&missing, ConfigScope::Local)
        .expect("io")
        .is_none());
}

#[test]
fn includeif_onbranch_misses_when_branch_differs() {
    let dir = tempdir().expect("tempdir");
    init_repository(
        dir.path(),
        false,
        "other",
        None,
        grit_lib::RefStorageFormat::Files,
    )
    .expect("init");
    let git_dir = dir.path().join(".git");
    std::fs::write(
        git_dir.join("config"),
        "[includeIf \"onbranch:main\"]\n\tpath = miss.conf\n",
    )
    .expect("cfg");
    std::fs::write(git_dir.join("miss.conf"), "[miss]\n\tk = 1\n").expect("miss");
    let env = Environment::empty();
    let set = ConfigSet::load_with_options(
        &env,
        Some(&git_dir),
        &grit_lib::config::LoadConfigOptions {
            include_system: false,
            process_includes: true,
            command_includes: true,
            include_ctx: grit_lib::config::IncludeContext {
                git_dir: Some(git_dir.clone()),
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .expect("load");
    assert!(set.get("miss.k").is_none());
}
