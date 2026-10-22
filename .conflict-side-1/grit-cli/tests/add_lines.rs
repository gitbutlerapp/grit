//! `grit add --lines` vs partial index updates.

use std::fs;
use std::process::Command;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit")
}

#[test]
fn add_lines_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("f.txt"), b"a\nb\nc\nd\n").expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), b"a\nB\nc\nD\n").expect("write");

    let add = run_grit(
        grit,
        root,
        &["add", "--lines", "f.txt:2-2", "--json", "--", "f.txt"],
    );
    assert!(
        add.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&add.stdout).expect("add json");
    assert_eq!(out["staged_lines"]["f.txt"], serde_json::json!(["2"]));
    assert_eq!(out["modified"], serde_json::json!(["f.txt"]));
}

#[test]
fn add_lines_pathspec_glob() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("f.txt"), b"a\nb\n").expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), b"a\nB\n").expect("write");

    let add = run_grit(
        grit,
        root,
        &["add", "--lines", "f.txt:2", "--json", "--", ":(glob)*.txt"],
    );
    assert!(
        add.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&add.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&add.stdout).expect("add json");
    assert_eq!(out["count"], 1);
}
