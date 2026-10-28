//! CLI tests for `grit blame -C` copy detection.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn git_env(cmd: &mut Command) {
    cmd.env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com");
}

fn git(repo: &Path, args: &[&str]) {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo).args(args);
    git_env(&mut cmd);
    assert!(cmd.status().unwrap().success(), "git {args:?}");
}

fn gs_json(repo: &Path, args: &[&str]) -> Value {
    let mut cmd = Command::new(GS);
    cmd.current_dir(repo).arg("--json").args(args);
    git_env(&mut cmd);
    let out = cmd.output().expect("grit");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn copy_score_line() -> String {
    "A".repeat(69)
}

fn setup_copy_by_creation(repo: &Path) {
    let long = copy_score_line();
    git(repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("one"), format!("{long}\n")).unwrap();
    git(repo, &["add", "one"]);
    git(repo, &["commit", "-m", "Initial"]);
    fs::write(repo.join("uno"), format!("{long}\n")).unwrap();
    git(repo, &["add", "uno"]);
    git(repo, &["commit", "-m", "Second"]);
}

#[test]
fn copy_detection_twice_reports_original_path_in_json() {
    let dir = tempfile::tempdir().unwrap();
    setup_copy_by_creation(dir.path());
    let grit = gs_json(dir.path(), &["blame", "-C", "-C", "uno"]);
    let line = &grit["lines"][0];
    assert_eq!(line["original_path"].as_str(), Some("one"));
    let mut git_cmd = Command::new("git");
    git_cmd
        .current_dir(dir.path())
        .args(["blame", "--porcelain", "-C", "-C", "--", "uno"]);
    git_env(&mut git_cmd);
    let git_out = git_cmd.output().unwrap();
    assert!(git_out.status.success());
    let porcelain = String::from_utf8_lossy(&git_out.stdout);
    assert!(porcelain.contains("filename one"));
    assert_eq!(
        line["commit"].as_str().map(str::len),
        Some(40),
        "expected full commit oid in JSON"
    );
}

#[test]
fn copy_detection_short_move_json_original_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    git(root, &["init", "-q", "-b", "main"]);
    let a: String = (1..=20).map(|n| format!("orig line {n}\n")).collect();
    fs::write(root.join("a.txt"), a).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "add a"]);
    let a_tail: String = (11..=20).map(|n| format!("orig line {n}\n")).collect();
    fs::write(root.join("a.txt"), a_tail).unwrap();
    let b: String = (1..=10)
        .map(|n| format!("orig line {n}\n"))
        .chain(std::iter::once("extra\n".to_owned()))
        .collect();
    fs::write(root.join("b.txt"), b).unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "move"]);
    let grit = gs_json(root, &["blame", "-C", "b.txt"]);
    let line = &grit["lines"][0];
    assert_eq!(line["original_path"].as_str(), Some("a.txt"));
}

#[test]
fn copy_detection_rejects_more_than_three_flags() {
    let dir = tempfile::tempdir().unwrap();
    setup_copy_by_creation(dir.path());
    let mut cmd = Command::new(GS);
    cmd.current_dir(dir.path())
        .args(["blame", "-C", "-C", "-C", "-C", "uno"]);
    git_env(&mut cmd);
    let out = cmd.output().expect("grit");
    assert!(
        out.status.code() == Some(2),
        "expected usage exit 2, got {:?} stderr={}",
        out.status.code(),
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("three") && stderr.contains("-C"),
        "unexpected stderr: {stderr}"
    );
}
