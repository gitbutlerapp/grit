//! `grit commit --hunks` partial commits.

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

fn two_hunk_file() -> (String, String) {
    let mut base = String::new();
    for i in 0..24 {
        base.push_str(&format!("line{i}\n"));
    }
    let mut edited = base.clone();
    edited = edited.replacen("line5\n", "LINE5\n", 1);
    edited = edited.replacen("line18\n", "LINE18\n", 1);
    (base, edited)
}

#[test]
fn commit_hunks_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    let (base, edited) = two_hunk_file();
    fs::write(root.join("f.txt"), base).expect("write");
    assert!(run_grit(grit, root, &["add", "f.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());
    fs::write(root.join("f.txt"), edited).expect("write");

    let diff = run_grit(grit, root, &["diff", "--unstaged", "--json"]);
    assert!(diff.status.success());
    let diff_json: serde_json::Value = serde_json::from_slice(&diff.stdout).expect("diff json");
    let id = diff_json["files"][0]["hunks"][0]["id"]
        .as_str()
        .expect("hunk id");

    let commit = run_grit(
        grit,
        root,
        &[
            "commit", "--hunks", id, "-m", "partial", "--json", "--", "f.txt",
        ],
    );
    assert!(
        commit.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&commit.stderr)
    );
    let out: serde_json::Value = serde_json::from_slice(&commit.stdout).expect("commit json");
    assert_eq!(out["scope"], "patch");
    assert_eq!(out["changes"], 1);
    assert!(out["oid"].as_str().is_some());
    assert_eq!(out["files"].as_array().map(|a| a.len()), Some(1));
}
