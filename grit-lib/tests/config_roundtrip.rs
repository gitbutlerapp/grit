//! Round-trip: values written by system `git config` are read through `repo.config()`.

use grit_lib::config::environment::Environment;
use grit_lib::config::{
    parse_path, parse_typed_value, ConfigEditor, ConfigScope, ConfigValueType, ConfigWriteOptions,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_test_support::git_fsck;
use std::path::{Path, PathBuf};
use std::process::Command;

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git_config(repo: &Repository, args: &[&str]) {
    let status = Command::new("git")
        .args(["-C"])
        .arg(repo.work_tree().expect("work tree"))
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .arg("config")
        .args(args)
        .status()
        .expect("git config");
    assert!(status.success(), "git config failed: {args:?}");
}

fn git_in_repo(repo_path: &Path, args: &[&str]) -> std::process::Output {
    Command::new("git")
        .args(["-C"])
        .arg(repo_path)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .args(args)
        .output()
        .expect("git")
}

#[test]
fn bool_int_path_read_through_repo_config() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default())?;

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
    let (repo, _) = Repository::init(dir.path(), &InitOptions::default()).expect("init");
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

#[test]
fn scoped_writes_round_trip_git_list_get_all_fsck() -> grit_lib::error::Result<()> {
    let tmp = tempfile::tempdir().expect("tempdir");
    let home = tmp.path().join("home");
    std::fs::create_dir_all(home.join(".config/git")).expect("xdg");
    let xdg_cfg = home.join(".config/git/config");
    std::fs::write(&xdg_cfg, "[user]\n\tname = xdg-user\n").expect("xdg global");

    let repo_path = tmp.path().join("repo");
    let (repo, _) = Repository::init(&repo_path, &InitOptions::default())?;

    let seed = git_in_repo(&repo_path, &["commit", "--allow-empty", "-m", "seed"]);
    assert!(
        seed.status.success(),
        "{}",
        String::from_utf8_lossy(&seed.stderr)
    );

    let wt = repo_path.join("wt");
    let wt_add = git_in_repo(&repo_path, &["worktree", "add", wt.to_str().expect("utf8")]);
    assert!(
        wt_add.status.success(),
        "{}",
        String::from_utf8_lossy(&wt_add.stderr)
    );

    let wt_repo = Repository::discover(
        Some(&wt),
        &grit_lib::config::environment::RepositoryOptions::empty(),
    )?;
    grit_lib::repo::init_worktree_config(wt_repo.git_dir())?;

    let mut env = Environment::empty();
    env.home = Some(home.as_os_str().to_os_string());
    env.xdg_config_home = Some(home.join(".config").display().to_string());

    let opts = ConfigWriteOptions::new();

    repo.config()
        .set(ConfigScope::Local, "grit.rp.local", "local-a", &opts)?;

    let editor = ConfigEditor::global(&env)?;
    editor.set("grit.rp.global", "global-a", &opts)?;

    wt_repo
        .config()
        .set(ConfigScope::Worktree, "grit.rp.worktree", "wt-a", &opts)?;

    git_config(&repo, &["--add", "grit.rp.multi", "first"]);
    git_config(&repo, &["--add", "grit.rp.multi", "second"]);

    let replace_opts = ConfigWriteOptions::new().replace_all(true);
    repo.config()
        .set(ConfigScope::Local, "grit.rp.multi", "third", &replace_opts)?;

    repo.reload_config()?;
    wt_repo.reload_config()?;

    let fsck = git_fsck(&repo_path, true);
    assert!(fsck.ok, "git fsck: {:?}", fsck.msg_ids);

    let list = Command::new("git")
        .args(["-C"])
        .arg(&repo_path)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .args(["config", "--list", "--show-scope"])
        .output()
        .expect("git config --list");
    assert!(
        list.status.success(),
        "{}",
        String::from_utf8_lossy(&list.stderr)
    );
    let text = String::from_utf8_lossy(&list.stdout);
    assert!(text.contains("local\tgrit.rp.local=local-a"), "{text}");

    let wt_list = Command::new("git")
        .args(["-C"])
        .arg(&wt)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .args(["config", "--list", "--show-scope"])
        .output()
        .expect("git config --list in worktree");
    assert!(
        wt_list.status.success(),
        "{}",
        String::from_utf8_lossy(&wt_list.stderr)
    );
    let wt_text = String::from_utf8_lossy(&wt_list.stdout);
    assert!(
        wt_text.contains("worktree\tgrit.rp.worktree=wt-a"),
        "{wt_text}"
    );

    let global_list = Command::new("git")
        .env("HOME", &home)
        .env("GIT_CONFIG_SYSTEM", null_device())
        .args(["config", "--global", "--list", "--show-scope"])
        .output()
        .expect("git global list");
    assert!(
        global_list.status.success(),
        "{}",
        String::from_utf8_lossy(&global_list.stderr)
    );
    let gtext = String::from_utf8_lossy(&global_list.stdout);
    assert!(gtext.contains("global\tgrit.rp.global=global-a"), "{gtext}");

    let get_all = Command::new("git")
        .args(["-C"])
        .arg(&repo_path)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .args(["config", "--get-all", "grit.rp.multi"])
        .output()
        .expect("get-all");
    assert!(get_all.status.success());
    assert_eq!(
        String::from_utf8_lossy(&get_all.stdout).trim(),
        "third",
        "replace-all should collapse multi-valued key"
    );

    assert_eq!(
        repo.config().get("grit.rp.local")?.as_deref(),
        Some("local-a")
    );

    Ok(())
}

#[test]
fn config_write_options_value_type_flows_to_config_file_set() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cfg");
    let env = Environment::empty();
    let editor = ConfigEditor::open(&env, path.clone(), ConfigScope::Local);

    let opts = ConfigWriteOptions::new().value_type(ConfigValueType::Bool);
    editor.set("sec.k", "on", &opts)?;

    let out = Command::new("git")
        .args(["config", "-f"])
        .arg(&path)
        .arg("--type")
        .arg("bool")
        .arg("--get")
        .arg("sec.k")
        .output()
        .expect("git");
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "true");
    Ok(())
}

#[test]
fn non_utf8_config_bytes_unchanged_when_edit_errors() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cfg");
    let bytes = b"[s]\n\tk = \xff\n";
    std::fs::write(&path, bytes).expect("write cfg");

    let env = Environment::empty();
    let editor = ConfigEditor::open(&env, path.clone(), ConfigScope::Local);
    let opts = ConfigWriteOptions::new();
    let err = editor
        .set("s.new", "value", &opts)
        .expect_err("non-UTF-8 config must not be overwritten");
    assert!(
        matches!(err, grit_lib::error::Error::Io(ref io) if io.kind() == std::io::ErrorKind::InvalidData),
        "expected InvalidData Io error, got {err:?}"
    );

    let after = std::fs::read(&path).expect("read back");
    assert_eq!(after.as_slice(), bytes);
}

#[test]
fn replace_value_pattern_selective_replace_git_get_all() -> grit_lib::error::Result<()> {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("cfg");
    std::fs::write(&path, "[sec]\n\tk = alpha\n\tk = beta\n\tk = alpha\n").expect("write");

    let env = Environment::empty();
    let editor = ConfigEditor::open(&env, path.clone(), ConfigScope::Local);
    let opts = ConfigWriteOptions::new()
        .replace_all(true)
        .replace_value_pattern("alpha");
    editor.set("sec.k", "gamma", &opts)?;

    let out = Command::new("git")
        .args(["config", "-f"])
        .arg(&path)
        .args(["--get-all", "sec.k"])
        .output()
        .expect("git");
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let lines: Vec<&str> = stdout.lines().map(str::trim).collect();
    // `git config --replace-all key val pattern` collapses matching lines; cross-check system git.
    assert_eq!(lines, vec!["beta", "gamma"]);

    Ok(())
}
