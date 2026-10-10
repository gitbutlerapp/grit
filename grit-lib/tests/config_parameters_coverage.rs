//! `GIT_CONFIG_PARAMETERS` strict parsing and command-layer merge coverage.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_lib::config::{
    git_config_parameters_last_value, parse_config_parameters, ConfigFile, ConfigSet,
    LoadConfigOptions,
};
use grit_lib::environment::Environment;

#[test]
fn from_git_config_parameters_covers_old_style_and_pairs() {
    let samples = [
        "'core.bare=true' 'user.name=Ada'",
        "'flag.enabled'",
        "'empty.value='",
        "'a.b=c' 'a.b=d'",
    ];
    for raw in samples {
        let file = ConfigFile::from_git_config_parameters(Path::new(":GIT_CONFIG_PARAMETERS"), raw)
            .unwrap_or_else(|e| panic!("parse {raw:?}: {e:?}"));
        let mut set = ConfigSet::new();
        set.merge(&file);
        assert!(!set.entries().is_empty());
    }
    let raw = "'core.bare=true' 'user.name=Bob'";
    assert_eq!(
        git_config_parameters_last_value(raw, "user.name").as_deref(),
        Some("Bob")
    );
    let round = parse_config_parameters(raw);
    assert!(!round.is_empty());
    assert!(ConfigFile::from_git_config_parameters(
        Path::new(":GIT_CONFIG_PARAMETERS"),
        "unquoted-without-quotes",
    )
    .is_err());
}

#[test]
fn load_merges_parameters_with_include_expansion() {
    let dir = tempfile::tempdir().expect("tempdir");
    let inc = dir.path().join("extra.conf");
    std::fs::write(&inc, "[frominc]\n\tk = 1\n").expect("inc");
    let mut env = Environment::empty();
    env.cwd = dir.path().to_path_buf();
    env.git_config_parameters = Some(format!(
        "'include.path={}' 'layer.k=top'",
        inc.display().to_string().replace('\\', "\\\\")
    ));
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
    assert_eq!(set.get("layer.k").as_deref(), Some("top"));
    assert_eq!(set.get("frominc.k").as_deref(), Some("1"));
}
