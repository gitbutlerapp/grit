//! Status JSON: typed conflicts and operation next_steps.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn null_device() -> &'static OsStr {
    if cfg!(windows) {
        OsStr::new("NUL")
    } else {
        OsStr::new("/dev/null")
    }
}

fn system_git_ok(repo: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn gs_json(repo: &Path, args: &[&str]) -> Value {
    let mut cmd_args: Vec<&str> = args.to_vec();
    cmd_args.push("--json");
    let out = Command::new(GS)
        .args(&cmd_args)
        .current_dir(repo)
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

/// Exit code 2 means clap usage error (unknown flag/subcommand).
fn assert_grit_command_parses(cmd: &str) {
    let words = shell_words::split(cmd).unwrap_or_else(|e| panic!("split {cmd:?}: {e}"));
    assert_eq!(words.first().map(String::as_str), Some("grit"));
    let out = Command::new(GS)
        .args(&words[1..])
        .output()
        .unwrap_or_else(|e| panic!("run {cmd:?}: {e}"));
    assert_ne!(
        out.status.code(),
        Some(2),
        "usage error for {cmd:?}:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn status_json_conflicts_and_operation_shape() -> TestResult {
    let tmp = tempfile::TempDir::new()?;
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f"), "base\n")?;
    system_git_ok(repo, &["add", "f"]);
    system_git_ok(repo, &["commit", "-qm", "base"]);
    system_git_ok(repo, &["checkout", "-qb", "side"]);
    fs::write(repo.join("f"), "side\n")?;
    system_git_ok(repo, &["commit", "-qam", "side"]);
    system_git_ok(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("f"), "main\n")?;
    system_git_ok(repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert_ne!(merge.status.code(), Some(0), "expected merge conflict");

    let v = gs_json(repo, &["status"]);
    assert_eq!(v["merging"], Value::Bool(true));
    assert_eq!(v["in_progress"], serde_json::json!(["merge"]));

    let conflicts = v["conflicts"].as_array().expect("conflicts array");
    assert_eq!(conflicts.len(), 1);
    assert_eq!(conflicts[0]["path"], "f");
    assert_eq!(conflicts[0]["kind"], "both_modified");

    let op = v.get("operation").expect("operation object");
    assert_eq!(op["kind"], "merge");
    let steps = op["next_steps"].as_array().expect("next_steps");
    assert!(!steps.is_empty());
    for step in steps {
        let cmd = step.as_str().expect("step string");
        let argv: Vec<&str> = cmd.split_whitespace().collect();
        assert_eq!(argv.first(), Some(&"grit"));
        assert_grit_command_parses(cmd);
    }

    Ok(())
}
