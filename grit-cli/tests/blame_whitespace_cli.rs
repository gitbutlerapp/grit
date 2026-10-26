//! CLI tests for `grit blame -w` / `--ignore-all-space`.

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

#[test]
fn ignore_all_space_flag_matches_git_on_whitespace_only_edit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init"]);
    fs::write(repo.join("file"), "alpha\n").unwrap();
    git(repo, &["add", "file"]);
    git(repo, &["commit", "-m", "A"]);
    let a_sha = Command::new("git")
        .current_dir(repo)
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let a = String::from_utf8_lossy(&a_sha.stdout).trim().to_owned();
    fs::write(repo.join("file"), "  alpha\n").unwrap();
    git(repo, &["add", "file"]);
    git(repo, &["commit", "-m", "reindent"]);

    let grit = gs_json(repo, &["blame", "file", "-w"]);
    let line0 = grit["lines"]
        .as_array()
        .and_then(|a| a.first())
        .expect("one line");
    assert_eq!(
        line0["commit"].as_str().expect("commit"),
        a,
        "with -w, whitespace-only reindent should attribute to original commit"
    );

    let mut git_cmd = Command::new("git");
    git_cmd
        .current_dir(repo)
        .args(["blame", "--porcelain", "-w", "--", "file"]);
    git_env(&mut git_cmd);
    let git_out = git_cmd.output().unwrap();
    assert!(git_out.status.success());
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let git_sha = git_stdout
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().next())
        .expect("git sha");
    assert_eq!(git_sha, a);
}

#[test]
fn final_newline_changes_attribute_json_line_to_modifying_commit() {
    let dir = tempfile::tempdir().unwrap();
    let repo = dir.path();
    git(repo, &["init", "-q", "-b", "main"]);
    for text in ["hello", "hello\n", "hello"] {
        fs::write(repo.join("a.txt"), text).unwrap();
        git(repo, &["add", "a.txt"]);
        git(repo, &["commit", "-qm", "change ending"]);
        let expected = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD"])
            .output()
            .unwrap();
        assert!(expected.status.success());
        let expected = String::from_utf8(expected.stdout).unwrap();
        git(repo, &["commit", "--allow-empty", "-qm", "unrelated"]);
        let result = gs_json(repo, &["blame", "--", "a.txt"]);
        assert_eq!(result["lines"].as_array().unwrap().len(), 1);
        assert_eq!(result["lines"][0]["commit"], expected.trim());
        assert_eq!(result["lines"][0]["content"], "hello");
        assert_eq!(result["lines"][0]["final_line"], 1);
    }
    git(repo, &["fsck", "--strict"]);
}
