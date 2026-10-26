//! Status JSON paths round-trip through blame without requiring raw argv (issue #2045).
#![cfg(unix)]
#![allow(clippy::unwrap_used)]

use grit_lib::repo_relpath::storage_bytes;
use grit_test_support::git;
use serde_json::Value;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::process::Command;

fn json(root: &std::path::Path, args: &[&str]) -> Value {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .arg("--json")
        .args(args)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn status_encoded_paths_feed_blame_lines_and_grouped_ranges() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Tool"]);
    git(root, &["config", "user.email", "tool@example.com"]);
    let raw = b"dir\xff/raw\xfe.txt";
    let file = root.join(OsStr::from_bytes(raw));
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(&file, "hello\nsecond\nthird\n").unwrap();
    git(root, &["add", "-A"]);
    git(root, &["commit", "-qm", "initial"]);
    std::fs::write(&file, "hello\nsecond\nthird\nmodified\n").unwrap();
    let status = json(root, &["status"]);
    let stored = status["unstaged"][0]["path"].as_str().unwrap();
    assert_eq!(storage_bytes(stored).as_ref(), raw);
    let blame = json(root, &["blame", stored]);
    assert_eq!(blame["file"], stored);
    assert_eq!(blame["lines"].as_array().unwrap().len(), 3);
    assert_eq!(blame["lines"][0]["content"], "hello");
    assert_eq!(
        storage_bytes(blame["lines"][0]["original_path"].as_str().unwrap()).as_ref(),
        raw
    );
    let ranged = json(root, &["blame", "-L", "/second/,+2", stored]);
    assert_eq!(ranged["lines"].as_array().unwrap().len(), 2);
    assert_eq!(ranged["lines"][0]["final_line"], 2);
    let grouped = json(root, &["blame", "--group", "-L", "2,3", stored]);
    assert!(grouped.get("lines").is_none());
    assert_eq!(grouped["hunks"][0]["final_start"], 2);
    assert_eq!(grouped["hunks"][0]["count"], 2);
    assert_eq!(
        storage_bytes(grouped["hunks"][0]["original_path"].as_str().unwrap()).as_ref(),
        raw
    );
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["--markdown", "blame", "-L", "2,3", stored])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8(out.stdout).unwrap().contains("second"));
}
