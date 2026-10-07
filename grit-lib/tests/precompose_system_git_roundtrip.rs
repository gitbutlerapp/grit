//! Precompose NFC index paths round-trip with system git (issue #910).

use std::fs;
use std::path::Path;

use grit_test_support::{git_cmd, grit_cmd, unique_tmp};
use tempfile::TempDir;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn grit_precompose_env(dir: &Path) -> grit_test_support::Cmd {
    grit_cmd(&["init"])
        .in_dir(dir)
        .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
}

#[test]
fn grit_stages_nfc_paths_readable_by_system_git() {
    let dir = TempDir::new().expect("tempdir");
    let root = dir.path();

    grit_precompose_env(root).suc();

    let nfd = format!("cafe\u{0301}.txt");
    fs::write(root.join(&nfd), b"hello\n").expect("write nfd worktree file");

    grit_cmd(&["add"])
        .in_dir(root)
        .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc();

    grit_cmd(&["commit", "-m", "nfc path"])
        .in_dir(root)
        .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc();

    let listed = git_cmd(&[
        "-c",
        "core.quotePath=false",
        "ls-tree",
        "-r",
        "HEAD",
        "--name-only",
    ])
    .in_dir(root)
    .suc()
    .stdout
    .trim()
    .to_owned();
    assert_eq!(
        listed.as_bytes(),
        b"caf\xc3\xa9.txt",
        "system git should read NFC UTF-8 path in commit tree"
    );

    let fsck = git_cmd(&["fsck", "--no-dangling"]).in_dir(root).suc();
    assert!(
        fsck.stderr.is_empty() || fsck.ok(),
        "git fsck failed: {}",
        fsck.dump("git fsck")
    );
}

#[test]
fn system_git_staged_nfc_index_readable_by_grit_status() {
    let scratch = unique_tmp("precompose", "git-to-grit");
    fs::create_dir_all(&scratch).expect("scratch");

    git_cmd(&["init"])
        .in_dir(&scratch)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .suc();

    git_cmd(&["config", "core.precomposeunicode", "true"])
        .in_dir(&scratch)
        .suc();

    let nfd = format!("cafe\u{0301}.txt");
    fs::write(scratch.join(&nfd), b"from git\n").expect("write");

    git_cmd(&["add", "--", &nfd])
        .in_dir(&scratch)
        .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
        .suc();

    let out = grit_cmd(&["status", "--json"])
        .in_dir(&scratch)
        .env("GIT_TEST_UTF8_NFD_TO_NFC", "1")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .exec();
    assert!(out.ok(), "grit status --json: {}", out.dump("grit status"));
    assert!(
        out.stdout.contains("\"unstaged\":[]") || out.stdout.contains("\"unstaged\": []"),
        "grit should not report false unstaged deletions for NFC index paths:\n{}",
        out.stdout
    );
}
