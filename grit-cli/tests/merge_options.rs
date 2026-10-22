//! `grit merge` option flags and JSON output.

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

fn setup_diverged(root: &Path) -> TestResult {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f"), "base\n")?;
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-c", "topic"]);
    fs::write(root.join("f"), "topic\n")?;
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "topic"]);
    git(root, &["checkout", "main"]);
    fs::write(root.join("g"), "main\n")?;
    git(root, &["add", "g"]);
    git(root, &["commit", "-qm", "main"]);
    Ok(())
}

fn setup_linear(root: &Path) -> TestResult {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f"), "a\n")?;
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-c", "topic"]);
    fs::write(root.join("f"), "b\n")?;
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "topic"]);
    git(root, &["switch", "main"]);
    Ok(())
}

#[test]
fn merge_ff_only_exit_code() -> TestResult {
    let dir = tempfile::tempdir()?;
    setup_diverged(dir.path())?;
    let out = grit_cmd(dir.path(), &["--json", "merge", "--ff-only", "topic"]);
    assert!(!out.ok, "expected failure");
    assert_eq!(out.code, 128);
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "refused_not_ff");
    Ok(())
}

#[test]
fn merge_squash_json() -> TestResult {
    let dir = tempfile::tempdir()?;
    setup_linear(dir.path())?;
    let out = grit_cmd(dir.path(), &["--json", "merge", "--squash", "topic"]);
    assert!(out.ok, "squash failed: {}", out.stdout);
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "squashed");
    assert!(json["next_steps"].is_array());
    Ok(())
}

#[test]
fn merge_no_commit_json() -> TestResult {
    let dir = tempfile::tempdir()?;
    setup_linear(dir.path())?;
    let out = grit_cmd(dir.path(), &["--json", "merge", "--no-commit", "topic"]);
    assert!(out.ok, "no-commit failed: {}", out.stdout);
    let json: Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["result"], "stopped");
    assert!(json["next_steps"].is_array());
    Ok(())
}
