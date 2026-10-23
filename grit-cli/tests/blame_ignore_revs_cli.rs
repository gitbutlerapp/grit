//! CLI tests for `grit blame` ignore-revision flags and JSON markers.

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

fn setup(repo: &Path) -> String {
    git(repo, &["init"]);
    fs::write(repo.join("file"), "line1\n").unwrap();
    git(repo, &["add", "file"]);
    git(repo, &["commit", "-m", "A"]);
    git(repo, &["tag", "A"]);
    fs::write(repo.join("file"), "line-one\n").unwrap();
    git(repo, &["add", "file"]);
    git(repo, &["commit", "-m", "X"]);
    git(repo, &["tag", "X"]);
    let x = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "X"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&x.stdout).trim().to_owned()
}

#[test]
fn ignore_rev_flag_matches_git_attribution() {
    let dir = tempfile::tempdir().unwrap();
    let x = setup(dir.path());
    let grit = gs_json(dir.path(), &["blame", "file", "--ignore-rev", &x]);
    let mut git_cmd = Command::new("git");
    git_cmd.current_dir(dir.path()).args([
        "blame",
        "--porcelain",
        "--ignore-rev",
        &x,
        "--",
        "file",
    ]);
    git_env(&mut git_cmd);
    let git_out = git_cmd.output().unwrap();
    assert!(git_out.status.success());
    let git_sha = String::from_utf8_lossy(&git_out.stdout)
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .unwrap()
        .to_owned();
    assert_eq!(grit["lines"][0]["commit"].as_str().unwrap(), git_sha);
}

#[test]
fn ignore_revs_file_and_config() {
    let dir = tempfile::tempdir().unwrap();
    let x = setup(dir.path());
    fs::write(dir.path().join("ignore_x"), format!("{x}\n")).unwrap();
    git(
        dir.path(),
        &["config", "--add", "blame.ignoreRevsFile", "ignore_x"],
    );
    let out = gs_json(dir.path(), &["blame", "file"]);
    let a = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "A"])
        .output()
        .unwrap();
    let a = String::from_utf8_lossy(&a.stdout).trim().to_owned();
    assert_eq!(out["lines"][0]["commit"].as_str().unwrap(), a);
}

#[test]
fn empty_ignore_revs_file_overrides_config() {
    let dir = tempfile::tempdir().unwrap();
    let x = setup(dir.path());
    fs::write(dir.path().join("ignore_x"), format!("{x}\n")).unwrap();
    git(
        dir.path(),
        &["config", "--add", "blame.ignoreRevsFile", "ignore_x"],
    );
    let with_config = gs_json(dir.path(), &["blame", "file"]);
    let head = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "A"])
        .output()
        .unwrap();
    let a = String::from_utf8_lossy(&head.stdout).trim().to_owned();
    assert_eq!(with_config["lines"][0]["commit"].as_str().unwrap(), a);
    let cleared = gs_json(dir.path(), &["blame", "file", "--ignore-revs-file", ""]);
    assert_eq!(cleared["lines"][0]["commit"].as_str().unwrap(), x);
}

#[test]
fn json_includes_ignored_and_unblamable_when_true() {
    let dir = tempfile::tempdir().unwrap();
    setup(dir.path());
    fs::write(
        dir.path().join("file"),
        "line-one-change\nline-two-changed\ny3\ny4\n",
    )
    .unwrap();
    git(dir.path(), &["add", "file"]);
    git(dir.path(), &["commit", "-m", "Y"]);
    git(dir.path(), &["tag", "Y"]);
    let y = Command::new("git")
        .current_dir(dir.path())
        .args(["rev-parse", "Y"])
        .output()
        .unwrap();
    let y = String::from_utf8_lossy(&y.stdout).trim().to_owned();
    let out = gs_json(dir.path(), &["blame", "file", "--ignore-rev", &y]);
    let line3 = &out["lines"][2];
    assert_eq!(line3["commit"].as_str().unwrap(), y);
    assert_eq!(line3.get("ignored"), None);
    assert_eq!(line3.get("unblamable"), Some(&Value::Bool(true)));
}
