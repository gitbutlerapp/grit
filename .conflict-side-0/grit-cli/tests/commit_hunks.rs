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

#[test]
fn commit_lines_with_pathspec() {
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

    let commit = run_grit(
        grit,
        root,
        &[
            "commit",
            "--lines",
            "f.txt:2-2",
            "-m",
            "line partial",
            "--json",
            "--",
            "f.txt",
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
}

#[test]
fn commit_hunks_and_lines_together() {
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
    let file = &diff_json["files"][0];
    let id = file["hunks"][0]["id"].as_str().expect("hunk id");
    let line18 = file["hunks"][1]["lines"]
        .as_array()
        .and_then(|lines| {
            lines
                .iter()
                .find(|l| l.get("kind").and_then(|k| k.as_str()) == Some("add"))
                .and_then(|l| l.get("new").and_then(|n| n.as_u64()))
        })
        .expect("new-side line for second hunk");

    let commit = run_grit(
        grit,
        root,
        &[
            "commit",
            "--hunks",
            id,
            "--lines",
            &format!("f.txt:{line18}"),
            "-m",
            "mixed partial",
            "--json",
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

    let unstaged = run_grit(grit, root, &["diff", "--unstaged"]);
    assert!(unstaged.status.success());
    let diff_text = String::from_utf8_lossy(&unstaged.stdout);
    assert!(
        !diff_text.is_empty(),
        "expected remaining unstaged edits after partial commit"
    );
}
