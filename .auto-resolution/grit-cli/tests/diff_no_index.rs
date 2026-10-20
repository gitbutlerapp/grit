//! CLI tests for `grit diff --no-index`.

use std::process::Command;

use grit_test_support::configure_git_env;
use tempfile::TempDir;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_diff_no_index(old: &str, new: &str) -> (i32, String) {
    let out = Command::new("git")
        .args(["diff", "--no-index", old, new])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("git");
    (
        out.status.code().unwrap_or(128),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn grit_diff_no_index(cwd: &std::path::Path, old: &str, new: &str) -> (i32, String) {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    let out = cmd
        .current_dir(cwd)
        .args(["diff", "--no-index", old, new])
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(128),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

#[test]
fn diff_no_index_outside_repository() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"1\n").expect("write");
    std::fs::write(&b, b"2\n").expect("write");
    let (code, out) = grit_diff_no_index(dir.path(), &a.to_string_lossy(), &b.to_string_lossy());
    assert_eq!(code, 1);
    assert!(out.contains("a.txt") || out.contains("-") || out.contains("+"));
}

#[test]
fn diff_no_index_exit_code_matches_git() {
    let dir = TempDir::new().expect("tempdir");
    let a = dir.path().join("a.txt");
    let b = dir.path().join("b.txt");
    std::fs::write(&a, b"same\n").expect("write");
    std::fs::write(&b, b"same\n").expect("write");
    let a_s = a.to_string_lossy().into_owned();
    let b_s = b.to_string_lossy().into_owned();
    let (git_code, _) = git_diff_no_index(&a_s, &b_s);
    let (grit_code, _) = grit_diff_no_index(dir.path(), &a_s, &b_s);
    assert_eq!(grit_code, git_code);

    std::fs::write(&b, b"different\n").expect("write");
    let (git_code, _) = git_diff_no_index(&a_s, &b_s);
    let (grit_code, _) = grit_diff_no_index(dir.path(), &a_s, &b_s);
    assert_eq!(grit_code, git_code);
    assert_eq!(grit_code, 1);
}
