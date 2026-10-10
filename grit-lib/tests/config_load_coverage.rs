//! Full config cascade, protected load, and `GIT_CONFIG_PARAMETERS` parsing coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::path::Path;
use std::sync::Arc;

use grit_lib::config::{
    git_config_parameters_last_value, parse_config_parameters, ConfigFile, ConfigScope, ConfigSet,
    LoadConfigOptions,
};
use grit_lib::diagnostics::CollectingDiagnostics;
use grit_lib::environment::Environment;
use grit_lib::error::Error;
use grit_lib::repo::init_repository;
use support::{default_load_opts, isolated_env};
use tempfile::tempdir;

#[test]
fn load_cascade_system_global_xdg_local_and_git_config_override() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let xdg = home.join(".config");
    fs::create_dir_all(xdg.join("git")).expect("xdg git dir");
    fs::write(xdg.join("git/config"), "[xdg]\n\tk = from-xdg\n").expect("xdg cfg");
    fs::write(
        home.join(".gitconfig"),
        "[global]\n\tk = from-home\n[layer]\n\thome = 1\n",
    )
    .expect("home cfg");

    let system = dir.path().join("system.gitconfig");
    fs::write(&system, "[system]\n\tk = from-system\n").expect("system");

    let repo = dir.path().join("repo");
    init_repository(&repo, false, "main", None, "files").expect("init");
    let git_dir = repo.join(".git");
    fs::write(
        git_dir.join("config"),
        "[local]\n\tk = from-local\n[remote \"origin\"]\n\turl = https://example.com/r.git\n",
    )
    .expect("local");

    let override_cfg = dir.path().join("override.conf");
    fs::write(&override_cfg, "[override]\n\tk = from-override\n").expect("override");

    let mut env = isolated_env(&home);
    env.git_config_nosystem = None;
    env.git_config_system = Some(system.display().to_string());
    env.git_config_global = None;
    env.xdg_config_home = Some(xdg.display().to_string());
    env.git_config = Some(override_cfg.display().to_string());
    env.cwd = repo.clone();

    let opts = LoadConfigOptions {
        include_system: true,
        ..default_load_opts(&git_dir, &env)
    };
    let set = ConfigSet::load_with_options(&env, Some(&git_dir), &opts).expect("load");
    assert_eq!(set.get("override.k").as_deref(), Some("from-override"));
    assert_eq!(set.get("local.k").as_deref(), Some("from-local"));
    assert_eq!(set.get("layer.home").as_deref(), Some("1"));
    assert!(set.get("system.k").is_some());
    assert!(set.get("xdg.k").is_some());
}

#[test]
fn load_protected_and_command_parameters() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    fs::create_dir_all(&home).expect("home");
    fs::write(home.join(".gitconfig"), "[protected]\n\tk = global\n").expect("global");

    let mut env = isolated_env(&home);
    env.git_config_global = Some(home.join(".gitconfig").display().to_string());
    env.git_config_count = Some("2".into());
    env.git_config_pairs = vec![("cmd.a".into(), "1".into()), ("cmd.b".into(), "2".into())];
    env.git_config_parameters = Some("'cmd.c=3' 'cmd.a=override'".into());

    let set = ConfigSet::load_protected(&env, false).expect("protected");
    assert_eq!(set.get("cmd.a").as_deref(), Some("override"));
    assert_eq!(set.get("cmd.b").as_deref(), Some("2"));
    assert_eq!(set.get("cmd.c").as_deref(), Some("3"));
    assert_eq!(set.get("protected.k").as_deref(), Some("global"));
}

#[test]
fn git_config_parameters_last_value_and_parse_roundtrip() {
    let raw = "'first.k=1' 'second.k=two' 'second.k=three' 'flag.enabled' 'pair.key='";
    assert_eq!(
        git_config_parameters_last_value(raw, "second.k").as_deref(),
        Some("three")
    );
    assert_eq!(
        git_config_parameters_last_value(raw, "flag.enabled").as_deref(),
        Some("true")
    );
    assert_eq!(
        git_config_parameters_last_value(raw, "missing").as_deref(),
        None
    );

    let round = parse_config_parameters(raw);
    assert!(round.iter().any(|e| e.contains("first.k")));
    assert!(parse_config_parameters("'key=value'")
        .iter()
        .any(|e| e.contains("key")));

    let err = ConfigFile::from_git_config_parameters(
        Path::new(":GIT_CONFIG_PARAMETERS"),
        "bad = 'unclosed",
    );
    assert!(err.is_err());
}

#[test]
fn worktree_config_layer_when_extension_enabled() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    fs::write(
        git_dir.join("config"),
        "[extensions]\n\tworktreeConfig = true\n[common]\n\tk = base\n",
    )
    .expect("common config");
    fs::write(git_dir.join("config.worktree"), "[wt]\n\tk = worktree\n").expect("wt config");

    let env = Environment::empty();
    let set =
        ConfigSet::load_with_options(&env, Some(&git_dir), &default_load_opts(&git_dir, &env))
            .expect("load");
    assert_eq!(set.get("wt.k").as_deref(), Some("worktree"));
    assert_eq!(set.get("common.k").as_deref(), Some("base"));
}

#[test]
fn load_with_command_includes_and_no_process_includes() {
    let dir = tempdir().expect("tempdir");
    let inc = dir.path().join("inc.conf");
    fs::write(&inc, "[frominc]\n\tk = yes\n").expect("inc");
    let mut env = Environment::empty();
    env.git_config_parameters = Some(format!(
        "'include.path={}'",
        inc.display().to_string().replace('\\', "\\\\")
    ));
    env.cwd = dir.path().to_path_buf();

    let opts = LoadConfigOptions {
        include_system: false,
        process_includes: false,
        command_includes: true,
        ..Default::default()
    };
    let without = ConfigSet::load_with_options(&env, None, &opts).expect("load");
    assert!(without.get("frominc.k").is_none());

    let opts_proc = LoadConfigOptions {
        process_includes: true,
        command_includes: true,
        include_system: false,
        include_ctx: grit_lib::config::IncludeContext {
            cwd: dir.path().to_path_buf(),
            env: std::sync::Arc::new(env.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let with = ConfigSet::load_with_options(&env, None, &opts_proc).expect("load proc");
    assert_eq!(with.get("frominc.k").as_deref(), Some("yes"));
}

#[test]
fn read_early_config_and_load_repo_local_only() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    fs::write(
        git_dir.join("config"),
        "[early]\n\tk = local-only\n[include]\n\tpath = missing.conf\n",
    )
    .expect("config");

    let env = Environment::empty();
    let early = ConfigSet::read_early_config(&env, Some(&git_dir), "early.k").expect("early");
    assert!(early.iter().any(|v| v == "local-only"));

    let local = ConfigSet::load_repo_local_only(&git_dir).expect("local only");
    assert_eq!(local.get("early.k").as_deref(), Some("local-only"));
}

#[test]
fn diagnostics_on_ignored_git_dir() {
    let dir = tempdir().expect("tempdir");
    let fake_git = dir.path().join("not-a-repo.git");
    fs::create_dir_all(&fake_git).expect("mkdir");

    let sink = Arc::new(CollectingDiagnostics::new());
    let env = Environment::empty();
    let opts = LoadConfigOptions {
        include_system: false,
        diagnostics: Some(sink.clone()),
        ..Default::default()
    };
    let set = ConfigSet::load_with_options(&env, Some(&fake_git), &opts).expect("load");
    assert!(set.get("core.bare").is_none());
    let _warnings = sink.warnings();
}

#[test]
fn load_surfaces_invalid_system_config() {
    let dir = tempdir().expect("tempdir");
    let bad = dir.path().join("bad-system");
    fs::write(&bad, "[broken\n").expect("write");
    let mut env = Environment::empty();
    env.git_config_system = Some(bad.display().to_string());
    let err = ConfigSet::load(&env, None, true).expect_err("bad system");
    assert!(matches!(err, Error::Config(_) | Error::Message(_)));
}

#[test]
fn load_skips_system_when_nosystem_set() {
    let dir = tempdir().expect("tempdir");
    let system = dir.path().join("system.conf");
    fs::write(&system, "[sys]\n\tk = 1\n").expect("sys");
    let mut env = Environment::empty();
    env.git_config_system = Some(system.display().to_string());
    env.git_config_nosystem = Some("true".into());
    let set = ConfigSet::load(&env, None, true).expect("load");
    assert!(set.get("sys.k").is_none());
}

#[test]
fn load_errors_on_invalid_repository_config() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    fs::write(git_dir.join("config"), "[bad\n").expect("bad");
    let env = Environment::empty();
    assert!(ConfigSet::load(&env, Some(&git_dir), false).is_err());
}

#[test]
fn load_errors_on_invalid_git_config_override_file() {
    let dir = tempdir().expect("tempdir");
    let bad = dir.path().join("override.conf");
    fs::write(&bad, "[broken\n").expect("write");
    let mut env = Environment::empty();
    env.git_config = Some(bad.display().to_string());
    env.cwd = dir.path().to_path_buf();
    let err = ConfigSet::load(&env, None, false).expect_err("bad override");
    assert!(matches!(err, Error::Config(_) | Error::Message(_)));
}

#[test]
fn worktree_config_not_loaded_without_extension() {
    let dir = tempdir().expect("tempdir");
    init_repository(dir.path(), false, "main", None, "files").expect("init");
    let git_dir = dir.path().join(".git");
    fs::write(git_dir.join("config.worktree"), "[wt]\n\tk = 1\n").expect("wt");
    let env = Environment::empty();
    let set =
        ConfigSet::load_with_options(&env, Some(&git_dir), &default_load_opts(&git_dir, &env))
            .expect("load");
    assert!(set.get("wt.k").is_none());
}

#[test]
fn command_relative_include_errors_when_configured() {
    let dir = tempdir().expect("tempdir");
    let inc = dir.path().join("inc.conf");
    fs::write(&inc, "[k]\n\tk = 1\n").expect("inc");
    let mut env = Environment::empty();
    env.git_config_parameters = Some("'include.path=inc.conf'".into());
    env.cwd = dir.path().to_path_buf();
    let opts = LoadConfigOptions {
        include_system: false,
        process_includes: true,
        command_includes: true,
        include_ctx: grit_lib::config::IncludeContext {
            cwd: dir.path().to_path_buf(),
            command_line_relative_include_is_error: true,
            env: std::sync::Arc::new(env.clone()),
            ..Default::default()
        },
        ..Default::default()
    };
    let err = ConfigSet::load_with_options(&env, None, &opts).expect_err("relative cmd include");
    assert!(matches!(err, Error::Config(_)));
}

#[test]
fn kitchen_sink_config_load_exercises_cascade_branches() {
    let dir = tempdir().expect("tempdir");
    let home = dir.path().join("home");
    let xdg = home.join(".config/git");
    fs::create_dir_all(&xdg).expect("xdg");
    fs::write(xdg.join("config"), "[xdg]\n\tk = xdg\n").expect("xdg");
    fs::write(home.join(".gitconfig"), "[g]\n\tk = global\n").expect("g");
    let system = dir.path().join("sys.conf");
    fs::write(&system, "[system]\n\tk = sys\n").expect("sys");
    let repo = dir.path().join("repo");
    init_repository(&repo, false, "feature/x", None, "files").expect("init");
    let git_dir = repo.join(".git");
    let mut cfg = fs::read_to_string(git_dir.join("config")).expect("read");
    cfg.push_str(
        "[extensions]\n\tworktreeConfig = true\n[includeIf \"onbranch:feature/x\"]\n\tpath = branch.conf\n",
    );
    fs::write(git_dir.join("config"), &cfg).expect("cfg");
    fs::write(git_dir.join("config.worktree"), "[wt]\n\tk = wt\n").expect("wt");
    fs::write(git_dir.join("branch.conf"), "[branch]\n\tk = hit\n").expect("branch");

    let mut env = isolated_env(&home);
    env.git_config_nosystem = None;
    env.git_config_system = Some(system.display().to_string());
    env.xdg_config_home = Some(home.join(".config").display().to_string());
    env.git_config_count = Some("1".into());
    env.git_config_pairs = vec![("pair.k".into(), "v".into())];
    env.git_config_parameters = Some("'param.k=p'".into());
    env.cwd = repo.clone();

    let set = ConfigSet::load(&env, Some(&git_dir), true).expect("kitchen");
    assert_eq!(set.get("branch.k").as_deref(), Some("hit"));
    assert_eq!(set.get("wt.k").as_deref(), Some("wt"));
    assert_eq!(set.get("param.k").as_deref(), Some("p"));
    assert_eq!(set.get("pair.k").as_deref(), Some("v"));
    let _early = ConfigSet::read_early_config(&env, Some(&git_dir), "branch.k").expect("early");
    let _protected = ConfigSet::load_protected(&env, false).expect("protected");
}

#[test]
fn optional_includeif_and_git_config_override_file() {
    let dir = tempdir().expect("tempdir");
    let cfg = dir.path().join("main.conf");
    fs::write(
        &cfg,
        "[includeIf \"gitdir:/nonexistent/path/\"]\n\tpath = opt.conf\n[set]\n\tx = 1\n",
    )
    .expect("write");
    fs::write(dir.path().join("opt.conf"), "[opt]\n\tk = 2\n").expect("opt");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();
    let file = ConfigFile::from_path(&cfg, ConfigScope::Local)
        .expect("read")
        .expect("exists");
    let mut set = ConfigSet::new();
    set.merge_file_with_includes(
        &file,
        true,
        &grit_lib::config::IncludeContext {
            cwd: dir.path().to_path_buf(),
            env: std::sync::Arc::new(env.clone()),
            ..Default::default()
        },
    )
    .expect("optional include");
    assert_eq!(set.get("set.x").as_deref(), Some("1"));

    let good = dir.path().join("override.conf");
    fs::write(&good, "[override]\n\tk = ok\n").expect("good");
    env.git_config = Some(good.display().to_string());
    let loaded = ConfigSet::load(&env, None, false).expect("override load");
    assert_eq!(loaded.get("override.k").as_deref(), Some("ok"));
}

#[test]
fn config_parse_errors_cover_negotiation_and_parameters() {
    let err = ConfigFile::parse(
        Path::new("c"),
        "[fetch]\n\tnegotiationAlgorithm\n",
        ConfigScope::Local,
    );
    assert!(matches!(
        err,
        Err(Error::Message(_)) | Err(Error::Config(_))
    ));

    let bogus = ConfigFile::from_git_config_parameters(
        Path::new(":GIT_CONFIG_PARAMETERS"),
        "'k=v' trailing",
    );
    assert!(bogus.is_err());
}
