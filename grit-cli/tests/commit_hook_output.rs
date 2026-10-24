//! `grit commit` shows a failing hook's output in every output mode (issue #1795).

#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const LINT_HOOK: &str =
    "#!/bin/sh\necho \"lint failed: bad stuff\" >&2\necho \"stdout from hook\"\nexit 1\n";

fn repo_with_failing_pre_commit() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let status = Command::new("git")
        .args(["init", "-q", "-b", "main"])
        .current_dir(tmp.path())
        .status()
        .expect("git init");
    assert!(status.success());
    let hook = tmp.path().join(".git/hooks/pre-commit");
    fs::create_dir_all(hook.parent().expect("hooks dir")).expect("mkdir");
    fs::write(&hook, LINT_HOOK).expect("write hook");
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755)).expect("chmod");
    fs::write(tmp.path().join("a"), "a\n").expect("write file");
    tmp
}

fn grit(dir: &Path, args: &[&str]) -> Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("run grit")
}

fn has_commits(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--verify", "-q", "HEAD"])
        .current_dir(dir)
        .status()
        .expect("git rev-parse")
        .success()
}

#[test]
fn human_mode_streams_hook_output_to_stderr_once() {
    let repo = repo_with_failing_pre_commit();
    let out = grit(repo.path(), &["commit", "-m", "one"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert_eq!(
        stderr.matches("lint failed: bad stuff").count(),
        1,
        "{stderr}"
    );
    assert_eq!(stderr.matches("stdout from hook").count(), 1, "{stderr}");
    assert!(
        stderr.contains("error: hook pre-commit failed with exit status 1"),
        "{stderr}"
    );
    assert!(!stdout.contains("stdout from hook"), "{stdout}");
    assert!(!has_commits(repo.path()));
}

#[test]
fn json_mode_puts_hook_output_in_the_error_object() {
    let repo = repo_with_failing_pre_commit();
    let out = grit(repo.path(), &["commit", "-m", "one", "--json"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(
        !stderr.contains("lint failed") && !stderr.contains("stdout from hook"),
        "captured hook output must not also reach stderr: {stderr}"
    );
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON object");
    assert_eq!(json["kind"], "hook_failed");
    assert_eq!(json["error"], "hook pre-commit failed with exit status 1");
    assert_eq!(json["hook"]["name"], "pre-commit");
    assert_eq!(json["hook"]["status"], 1);
    assert_eq!(json["hook"]["stdout"], "stdout from hook\n");
    assert_eq!(json["hook"]["stderr"], "lint failed: bad stuff\n");
    assert!(!has_commits(repo.path()));
}

#[test]
fn markdown_mode_renders_hook_output_blocks() {
    let repo = repo_with_failing_pre_commit();
    let out = grit(repo.path(), &["commit", "-m", "one", "--markdown"]);
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(out.status.code(), Some(1), "{stdout}\n{stderr}");
    assert!(!stderr.contains("lint failed"), "{stderr}");
    assert_eq!(
        stdout,
        "**Error:** hook pre-commit failed with exit status 1\n\
         \n**Hook stdout:**\n\n```text\nstdout from hook\n```\n\
         \n**Hook stderr:**\n\n```text\nlint failed: bad stuff\n```\n"
    );
}
