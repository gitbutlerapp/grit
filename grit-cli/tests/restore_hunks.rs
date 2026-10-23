//! `grit restore --hunks` JSON output.

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
fn restore_hunks_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("f.txt"), b"one\n").expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), b"two\n").expect("write");

    let diff = run_grit(grit, root, &["diff", "--unstaged", "--json"]);
    assert!(diff.status.success());
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let id = diff_json["files"][0]["hunks"][0]["id"]
        .as_str()
        .expect("hunk id");

    let restore = run_grit(
        grit,
        root,
        &["restore", "--hunks", id, "--json", "--", "f.txt"],
    );
    assert!(
        restore.status.success(),
        "{}",
        String::from_utf8_lossy(&restore.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&restore.stdout).expect("restore json");
    assert_eq!(out["restored_hunks"]["f.txt"], serde_json::json!([id]));
    assert_eq!(out["restored"], serde_json::json!(["f.txt"]));
}
