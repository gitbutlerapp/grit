//! Round-trip: values written by system `git config` are read through `repo.config()`.

use grit_lib::config::{parse_path, parse_typed_value, ConfigValueType};
use grit_lib::repo::{InitOptions, Repository};
use std::path::PathBuf;
use std::process::Command;

fn git_config(repo: &Repository, args: &[&str]) {
    let status = Command::new("git")
        .args(["-C"])
        .arg(repo.work_tree().expect("work tree"))
        .arg("config")
        .args(args)
        .status()
        .expect("git config");
    assert!(status.success(), "git config failed: {args:?}");
}

#[test]
fn bool_int_path_read_through_repo_config() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default())?;

    git_config(&repo, &["user.name", "Roundtrip"]);
    git_config(&repo, &["grit.testbool", "yes"]);
    git_config(&repo, &["grit.testint", "2k"]);
    git_config(&repo, &["grit.testpath", "~/cfg-path"]);

    repo.reload_config()?;

    let cfg = repo.config();
    assert_eq!(cfg.get("user.name")?.as_deref(), Some("Roundtrip"));
    assert_eq!(cfg.get_bool("grit.testbool")?, Some(true));
    assert_eq!(cfg.get_i64("grit.testint")?, Some(2_048));
    let env = repo.environment();
    let raw = cfg.get("grit.testpath")?.expect("raw path");
    assert_eq!(raw, "~/cfg-path");
    let path = cfg.get_path("grit.testpath")?.expect("path");
    assert_eq!(path, PathBuf::from(parse_path(env, &raw)));

    let env = env.clone();
    assert_eq!(
        parse_typed_value("grit.testbool", ConfigValueType::Bool, "on", &env)?,
        grit_lib::config::TypedConfigValue::Bool(true)
    );

    Ok(())
}

#[test]
fn invalid_bool_yields_config_error() {
    let dir = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
    git_config(&repo, &["grit.badbool", "maybe"]);
    repo.reload_config().expect("reload");

    let err = repo
        .config()
        .get_bool("grit.badbool")
        .expect_err("invalid bool");
    assert!(matches!(
        err,
        grit_lib::error::Error::Config(grit_lib::error::ConfigError::InvalidValue { .. })
    ));
}
