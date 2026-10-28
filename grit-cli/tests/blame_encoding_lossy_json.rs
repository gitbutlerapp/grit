//! `grit blame --json` marks non-UTF-8 lines with `encoding_lossy` (issue #2260).
#![cfg(feature = "full-tests")]
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "grit {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

const LATIN1_FILE: &[u8] = &[
    0x63, 0x61, 0x66, 0xe9, 0x0a, 0x6e, 0x61, 0xef, 0x76, 0x65, 0x0a,
];

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "D")
        .env("GIT_AUTHOR_EMAIL", "d@e.com")
        .env("GIT_COMMITTER_NAME", "D")
        .env("GIT_COMMITTER_EMAIL", "d@e.com")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn blame_json_encoding_lossy_on_latin1_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("l1.txt"), LATIN1_FILE).unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "latin1"]);

    let json = grit_json(root, &["blame", "l1.txt", "--json"]);
    let lines = json["lines"].as_array().expect("lines array");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["content"], "caf\u{fffd}");
    assert_eq!(lines[0]["encoding_lossy"], true);
    assert_eq!(lines[1]["content"], "na\u{fffd}ve");
    assert_eq!(lines[1]["encoding_lossy"], true);
}

#[test]
fn blame_json_mixed_encoding_per_line_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("mixed.txt"), b"plain ascii\ncaf\xe9\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "mixed"]);

    let json = grit_json(root, &["blame", "mixed.txt", "--json"]);
    let lines = json["lines"].as_array().expect("lines");
    assert_eq!(lines.len(), 2);
    assert_eq!(lines[0]["content"], "plain ascii");
    assert!(lines[0].get("encoding_lossy").is_none());
    assert_eq!(lines[1]["content"], "caf\u{fffd}");
    assert_eq!(lines[1]["encoding_lossy"], true);
}

#[test]
fn blame_json_group_mixed_encoding_line_flags() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "core.autocrlf", "false"]);
    std::fs::write(root.join("mixed.txt"), b"plain ascii\ncaf\xe9\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "mixed"]);

    let json = grit_json(root, &["blame", "mixed.txt", "--json", "--group"]);
    let hunks = json["hunks"].as_array().expect("hunks");
    assert_eq!(hunks.len(), 1);
    assert_eq!(hunks[0]["lines"].as_array().unwrap().len(), 2);
    let flags = hunks[0]["line_encoding_lossy"]
        .as_array()
        .expect("line_encoding_lossy");
    assert_eq!(flags.len(), 2);
    assert_eq!(flags[0], false);
    assert_eq!(flags[1], true);
}
