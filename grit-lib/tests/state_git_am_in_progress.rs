//! `git am` mid-session on-disk state is visible to grit-lib (`rebase-apply/applying`).

use std::fs;
use std::path::Path;
use std::process::Command;

use grit_lib::state::{detect_in_progress, resolve_head, wt_status_get_state, InProgressOperation};
use tempfile::TempDir;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

/// Run `git` in `dir`; returns `None` if git is unavailable or the command failed to start.
fn git(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .ok()
}

fn git_ok(dir: &Path, args: &[&str]) -> bool {
    matches!(git(dir, args), Some(out) if out.status.success())
}

/// Build a repo where `git am` stops with a failed patch and `rebase-apply/applying` present.
fn setup_failed_git_am(dir: &Path) -> bool {
    if !git_ok(dir, &["init", "-q"]) {
        return false;
    }
    fs::write(dir.join("x"), b"1\n").expect("write x");
    if !git_ok(dir, &["add", "x"]) || !git_ok(dir, &["commit", "-q", "-m", "one"]) {
        return false;
    }
    fs::write(dir.join("x"), b"2\n").expect("write x");
    if !git_ok(dir, &["add", "x"]) || !git_ok(dir, &["commit", "-q", "-m", "two"]) {
        return false;
    }
    let patch = git(dir, &["format-patch", "-1", "--stdout"]);
    let Some(patch) = patch else {
        return false;
    };
    if !patch.status.success() {
        return false;
    }
    let patch_bytes = patch.stdout;
    if !git_ok(dir, &["reset", "--hard", "-q", "HEAD~1"]) {
        return false;
    }
    fs::write(dir.join("x"), b"9\n").expect("write x");
    if !git_ok(dir, &["add", "x"]) || !git_ok(dir, &["commit", "-q", "-m", "nine"]) {
        return false;
    }
    let patch_path = dir.join("conflict.patch");
    fs::write(&patch_path, &patch_bytes).expect("write patch");
    let am = git(dir, &["am", patch_path.to_str().expect("utf8 path")]);
    let Some(am) = am else {
        return false;
    };
    if am.status.success() {
        return false;
    }
    dir.join(".git")
        .join("rebase-apply")
        .join("applying")
        .is_file()
}

#[test]
fn system_git_am_conflict_reports_in_progress_state() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();
    if !setup_failed_git_am(root) {
        eprintln!("skipping: system git unavailable or could not create am session");
        return;
    }

    let git_dir = root.join(".git");
    let ops = detect_in_progress(&git_dir);
    assert!(
        ops.contains(&InProgressOperation::Am),
        "expected Am in {:?}, got {:?}",
        git_dir,
        ops
    );

    let head = resolve_head(&git_dir).expect("resolve HEAD");
    let wt = wt_status_get_state(&git_dir, &head, false).expect("wt status state");
    assert!(
        wt.am_in_progress,
        "expected am_in_progress in wt status state: {:?}",
        wt
    );
}
