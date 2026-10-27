//! Output modes and thread counts agree on large worktrees containing symlinks.
#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::fs;
use std::os::unix::fs::symlink;
use std::path::Path;
use std::process::Command;

use grit_test_support::git;

fn diff(root: &Path, threads: &str, output: Option<&str>) -> String {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_grit"));
    cmd.current_dir(root)
        .args(["--threads", threads, "diff"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null");
    if let Some(flag) = output {
        cmd.arg(flag);
    }
    let result = cmd.output().expect("grit diff");
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).expect("stdout")
}

#[test]
fn parallel_symlinks_are_clean_and_render_retargets_in_every_output() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let root = tmp.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "Test"]);
    git(root, &["config", "user.email", "test@example.com"]);
    fs::create_dir(root.join("files")).expect("directory");
    for n in 1..=1024 {
        fs::write(root.join(format!("files/{n}")), b"data\n").expect("file");
    }
    for (path, target) in [
        ("linkdir", "files"),
        ("linkfile", "files/1"),
        ("linkmissing", "missing"),
    ] {
        symlink(target, root.join(path)).expect("symlink");
    }
    git(root, &["add", "."]);
    git(root, &["commit", "-qm", "initial"]);
    // Clear stat caches deterministically so every command verifies the link contents.
    git(root, &["read-tree", "HEAD"]);
    assert!(git(root, &["diff"]).is_empty());
    for output in [None, Some("--json"), Some("--markdown")] {
        let serial = diff(root, "1", output);
        assert_eq!(diff(root, "4", output), serial);
        if output == Some("--json") {
            let json: serde_json::Value = serde_json::from_str(&serial).expect("JSON");
            assert_eq!(json["changed"], false);
            assert_eq!(json["files"], serde_json::json!([]));
        }
    }
    for (path, target) in [
        ("linkdir", "."),
        ("linkfile", "files/2"),
        ("linkmissing", "absent"),
    ] {
        fs::remove_file(root.join(path)).expect("remove link");
        symlink(target, root.join(path)).expect("retarget");
    }
    assert_eq!(git(root, &["diff", "--name-only"]).lines().count(), 3);
    for output in [None, Some("--json"), Some("--markdown")] {
        let serial = diff(root, "1", output);
        assert_eq!(diff(root, "4", output), serial);
        for path in ["linkdir", "linkfile", "linkmissing"] {
            assert!(serial.contains(path), "{serial}");
        }
        if output == Some("--json") {
            let json: serde_json::Value = serde_json::from_str(&serial).expect("JSON");
            assert_eq!(json["changed"], true);
            assert_eq!(json["files"].as_array().expect("files").len(), 3);
            for file in json["files"].as_array().expect("files") {
                assert_eq!(file["status"], "modified");
                assert!(!file["hunks"].as_array().expect("hunks").is_empty());
            }
        }
    }
    git(root, &["fsck", "--strict"]);
}
