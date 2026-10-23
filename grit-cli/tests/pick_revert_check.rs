//! `grit pick` / `grit revert --check` exit codes and JSON.

use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

struct CmdOutput {
    ok: bool,
    stdout: String,
    code: i32,
}

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit_cmd(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("grit");
    CmdOutput {
        ok: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        code: out.status.code().unwrap_or(-1),
    }
}

fn setup_pick_clean(root: &Path) -> TestResult {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("base.txt"), "base\n")?;
    git(root, &["add", "base.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-c", "topic"]);
    fs::write(root.join("only-topic.txt"), "topic\n")?;
    git(root, &["add", "only-topic.txt"]);
    git(root, &["commit", "-qm", "topic only"]);
    git(root, &["switch", "main"]);
    Ok(())
}

fn setup_pick_conflict(root: &Path) -> TestResult {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f.txt"), "base\n")?;
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-c", "topic"]);
    fs::write(root.join("f.txt"), "topic\n")?;
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "topic"]);
    git(root, &["switch", "main"]);
    fs::write(root.join("f.txt"), "main\n")?;
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "main"]);
    Ok(())
}

fn setup_revert_clean(root: &Path) -> TestResult {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a.txt"), "a\n")?;
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "first"]);
    fs::write(root.join("b.txt"), "b\n")?;
    git(root, &["add", "b.txt"]);
    git(root, &["commit", "-qm", "second"]);
    Ok(())
}

#[test]
fn pick_check_exit_codes() -> TestResult {
    let clean_dir = tempfile::tempdir()?;
    setup_pick_clean(clean_dir.path())?;
    let clean = grit_cmd(clean_dir.path(), &["--json", "pick", "--check", "topic"]);
    assert!(clean.ok, "expected clean check: {}", clean.stdout);
    assert_eq!(clean.code, 0);
    let v: Value = serde_json::from_str(&clean.stdout)?;
    assert_eq!(v["result"], "clean");
    assert_eq!(v["check"], true);
    assert!(v["oid"].as_str().is_some_and(|s| !s.is_empty()));

    let conflict_dir = tempfile::tempdir()?;
    setup_pick_conflict(conflict_dir.path())?;
    let conflict = grit_cmd(conflict_dir.path(), &["--json", "pick", "--check", "topic"]);
    assert!(!conflict.ok);
    assert_eq!(conflict.code, 1);
    let v: Value = serde_json::from_str(&conflict.stdout)?;
    assert_eq!(v["result"], "conflicts");
    assert_eq!(v["check"], true);
    assert!(v["conflicts"].as_array().is_some_and(|a| !a.is_empty()));
    Ok(())
}

#[test]
fn revert_check_exit_codes() -> TestResult {
    let clean_dir = tempfile::tempdir()?;
    setup_revert_clean(clean_dir.path())?;
    let head = git(clean_dir.path(), &["rev-parse", "HEAD"])
        .trim()
        .to_owned();
    let clean = grit_cmd(clean_dir.path(), &["--json", "revert", "--check", &head]);
    assert!(clean.ok, "expected clean revert check: {}", clean.stdout);
    assert_eq!(clean.code, 0);
    let v: Value = serde_json::from_str(&clean.stdout)?;
    assert_eq!(v["result"], "clean");
    assert_eq!(v["check"], true);

    let conflict_dir = tempfile::tempdir()?;
    setup_pick_conflict(conflict_dir.path())?;
    let topic = git(conflict_dir.path(), &["rev-parse", "topic"])
        .trim()
        .to_owned();
    let conflict = grit_cmd(
        conflict_dir.path(),
        &["--json", "revert", "--check", &topic],
    );
    assert!(!conflict.ok);
    assert_eq!(conflict.code, 1);
    let v: Value = serde_json::from_str(&conflict.stdout)?;
    assert_eq!(v["result"], "conflicts");
    assert_eq!(v["check"], true);
    Ok(())
}
