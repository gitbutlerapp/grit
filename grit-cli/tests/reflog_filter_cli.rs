//! `grit reflog` list filters.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn init_repo() -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("tempdir");
    Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .status()
        .expect("git init");
    Command::new("git")
        .args(["config", "user.email", "t@e.c"])
        .current_dir(dir.path())
        .status()
        .expect("config");
    Command::new("git")
        .args(["config", "user.name", "T"])
        .current_dir(dir.path())
        .status()
        .expect("config");
    for i in 0..5 {
        std::fs::write(dir.path().join("f"), format!("{i}")).expect("write");
        Command::new("git")
            .args(["add", "f"])
            .current_dir(dir.path())
            .status()
            .expect("add");
        Command::new("git")
            .args(["commit", "-qm", &format!("m{i}")])
            .current_dir(dir.path())
            .status()
            .expect("commit");
    }
    dir
}

#[test]
fn reflog_json_max_count_and_truncated() {
    let dir = init_repo();
    let v = grit_json(dir.path(), &["reflog", "-n", "2"]);
    assert_eq!(v["entries"].as_array().expect("entries").len(), 2);
    assert_eq!(v["truncated"].as_bool(), Some(true));
    assert!(v["entries"][0]["action"].as_str().is_some());
}

#[test]
fn reflog_grep_and_since_flags() {
    let dir = init_repo();
    let grep = grit_json(dir.path(), &["reflog", "--grep=commit", "-n", "10"]);
    assert!(grep["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .all(|e| e["message"].as_str().unwrap_or("").contains("commit")));
    let since = grit_json(dir.path(), &["reflog", "--since=1970-01-01"]);
    assert!(!since["entries"].as_array().expect("entries").is_empty());
}

#[test]
fn reflog_max_count_zero_returns_empty_json() {
    let dir = init_repo();
    let v = grit_json(dir.path(), &["reflog", "-n", "0"]);
    assert!(v["entries"].as_array().expect("entries").is_empty());
}

#[test]
fn reflog_limit_alias_still_works() {
    let dir = init_repo();
    let v = grit_json(dir.path(), &["reflog", "--limit", "1"]);
    assert_eq!(v["entries"].as_array().expect("entries").len(), 1);
}
