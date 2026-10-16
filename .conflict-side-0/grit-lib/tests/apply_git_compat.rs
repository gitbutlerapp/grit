//! Patch parsing vs `git apply --check` on git-generated unified diffs.

use std::process::Command;

use grit_lib::apply::parse_patch;
use tempfile::TempDir;

fn git_diff(root: &std::path::Path) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(["diff"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git diff");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).to_string()
}

#[test]
fn apply_parse_accepts_git_worktree_diff() {
    let dir = TempDir::new().expect("tempdir");
    Command::new("git")
        .current_dir(dir.path())
        .args(["init", "-q", "-b", "main", "."])
        .status()
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), b"one\n").unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["add", "a.txt"])
        .status()
        .unwrap();
    Command::new("git")
        .current_dir(dir.path())
        .args(["commit", "-qm", "c"])
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .status()
        .unwrap();
    std::fs::write(dir.path().join("a.txt"), b"two\n").unwrap();
    let patch = git_diff(dir.path());
    assert!(!patch.is_empty(), "expected non-empty worktree diff");
    let files = parse_patch(&patch, 1, "patch", false, None).expect("parse");
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].new_path.as_deref(), Some("a.txt"));
}
