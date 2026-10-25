//! `grit diff --json` and `grit status --json` report a file -> symlink change as
//! `type_changed` in every diff mode (issue #1843), and the committed result
//! passes `git fsck`.
#![cfg(unix)]
#![allow(clippy::expect_used)]

use std::path::Path;
use std::process::Command;

const NULL_CONFIG: &str = "/dev/null";

fn run(program: &str, root: &Path, args: &[&str]) -> String {
    let out = Command::new(program)
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", NULL_CONFIG)
        .env("GIT_CONFIG_SYSTEM", NULL_CONFIG)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|e| panic!("{program} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).expect("utf8 stdout")
}

fn grit(root: &Path, args: &[&str]) -> String {
    run(env!("CARGO_BIN_EXE_grit"), root, args)
}

fn git(root: &Path, args: &[&str]) -> String {
    run("git", root, args)
}

/// `(path, status)` for every `files[]` entry of `grit diff <args> --json`.
fn diff_statuses(root: &Path, args: &[&str]) -> Vec<(String, String)> {
    let mut full = vec!["diff"];
    full.extend(args);
    full.push("--json");
    let v: serde_json::Value = serde_json::from_str(&grit(root, &full)).expect("diff json");
    v["files"]
        .as_array()
        .expect("files array")
        .iter()
        .map(|f| {
            (
                f["path"].as_str().expect("path").to_owned(),
                f["status"].as_str().expect("status").to_owned(),
            )
        })
        .collect()
}

fn status_of(root: &Path, section: &str) -> Vec<(String, String)> {
    let v: serde_json::Value =
        serde_json::from_str(&grit(root, &["status", "--json"])).expect("status json");
    v[section]
        .as_array()
        .expect("section array")
        .iter()
        .map(|f| {
            (
                f["path"].as_str().expect("path").to_owned(),
                f["status"].as_str().expect("status").to_owned(),
            )
        })
        .collect()
}

fn one(path: &str, status: &str) -> Vec<(String, String)> {
    vec![(path.to_owned(), status.to_owned())]
}

#[test]
fn file_to_symlink_is_type_changed_in_every_diff_mode() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    grit(root, &["init", "-b", "main"]);
    std::fs::write(root.join("f.txt"), "t\n").expect("write");
    std::fs::write(root.join("target.txt"), "c\n").expect("write");
    grit(root, &["commit", "-m", "a"]);

    std::fs::remove_file(root.join("f.txt")).expect("rm");
    std::os::unix::fs::symlink("target.txt", root.join("f.txt")).expect("symlink");

    assert_eq!(
        diff_statuses(root, &["--unstaged"]),
        one("f.txt", "type_changed")
    );
    assert_eq!(diff_statuses(root, &[]), one("f.txt", "type_changed"));
    assert_eq!(status_of(root, "unstaged"), one("f.txt", "type_changed"));

    grit(root, &["add"]);
    assert_eq!(
        diff_statuses(root, &["--staged"]),
        one("f.txt", "type_changed")
    );
    assert_eq!(status_of(root, "staged"), one("f.txt", "type_changed"));
    assert_eq!(
        git(root, &["diff", "--cached", "--name-status"]),
        "T\tf.txt\n"
    );

    grit(root, &["commit", "-m", "b"]);
    assert_eq!(
        diff_statuses(root, &["HEAD~1", "HEAD"]),
        one("f.txt", "type_changed")
    );
    git(root, &["fsck", "--strict"]);
    assert_eq!(
        git(root, &["ls-tree", "HEAD", "f.txt"])
            .split_whitespace()
            .next(),
        Some("120000")
    );
}

#[test]
fn chmod_only_stays_modified_in_every_diff_mode() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    grit(root, &["init", "-b", "main"]);
    std::fs::write(root.join("run.sh"), "echo hi\n").expect("write");
    grit(root, &["commit", "-m", "a"]);

    let path = root.join("run.sh");
    let mut perms = std::fs::metadata(&path).expect("meta").permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(&path, perms).expect("chmod");

    assert_eq!(
        diff_statuses(root, &["--unstaged"]),
        one("run.sh", "modified")
    );
    grit(root, &["add"]);
    assert_eq!(
        diff_statuses(root, &["--staged"]),
        one("run.sh", "modified")
    );
    grit(root, &["commit", "-m", "b"]);
    assert_eq!(
        diff_statuses(root, &["HEAD~1", "HEAD"]),
        one("run.sh", "modified")
    );
}
