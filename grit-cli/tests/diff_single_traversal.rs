//! `grit diff` must scan the index/worktree once per invocation for patch/stat paths.

use std::process::Command;

use grit_test_support::cli_fixture::CliFixture;
use grit_test_support::{git, grit};

fn scan_count(path: &std::path::Path) -> usize {
    std::fs::read_to_string(path)
        .map(|s| s.lines().count())
        .unwrap_or(0)
}

fn grit_with_scan_env(
    dir: &std::path::Path,
    args: &[&str],
    counter_file: &std::path::Path,
) -> bool {
    Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .env("GRIT_TEST_DIFF_ENTRY_SCAN_FILE", counter_file)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .args(args)
        .status()
        .expect("run grit")
        .success()
}

#[test]
fn diff_staged_patch_single_index_scan() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("f.txt"), b"base\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("f.txt"), b"staged\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);

    let counter_file = fx.path().join("scan.count");
    let _ = std::fs::remove_file(&counter_file);
    assert!(
        grit_with_scan_env(fx.path(), &["diff", "--staged"], &counter_file),
        "staged diff failed"
    );
    assert_eq!(
        scan_count(&counter_file),
        1,
        "expected one index-to-tree scan for staged patch diff"
    );
}

#[test]
fn diff_unstaged_patch_single_worktree_scan() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("f.txt"), b"base\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("f.txt"), b"dirty\n").expect("write");

    let counter_file = fx.path().join("scan.count");
    let _ = std::fs::remove_file(&counter_file);
    assert!(
        grit_with_scan_env(fx.path(), &["diff", "--unstaged"], &counter_file),
        "unstaged diff failed"
    );
    assert_eq!(
        scan_count(&counter_file),
        1,
        "expected one index-to-worktree scan for unstaged patch diff"
    );
}

#[test]
fn diff_default_patch_uses_grit_binary() {
    let fx = CliFixture::new();
    std::fs::write(fx.path().join("f.txt"), b"base\n").expect("write");
    git(fx.path(), &["add", "f.txt"]);
    git(fx.path(), &["commit", "-m", "init"]);
    std::fs::write(fx.path().join("f.txt"), b"dirty\n").expect("write");
    let out = grit(fx.path(), &["diff"]);
    assert!(out.ok(), "{}", out.dump("default diff"));
}
