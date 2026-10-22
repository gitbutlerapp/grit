//! `grit add --patch-file` vs `git apply --cached`.

use std::fs;
use std::io::Write;
use std::process::{Command, Stdio};

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(grit: &str, dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(grit)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit")
}

#[test]
fn add_patch_file_stdin_json() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("a.txt"), b"one\n").expect("write");
    assert!(run_grit(grit, root, &["add", "a.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());

    let patch = b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n";
    let mut child = Command::new(grit)
        .current_dir(root)
        .args(["add", "--patch-file", "-", "--json"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .spawn()
        .expect("spawn");
    child
        .stdin
        .take()
        .expect("stdin")
        .write_all(patch)
        .expect("write patch");
    let out = child.wait_with_output().expect("wait");
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json stdout");
    assert_eq!(json["modified"], serde_json::json!(["a.txt"]));
    assert_eq!(json["dry_run"], false);
}

#[test]
fn add_patch_file_dry_run_leaves_index() {
    let grit = env!("CARGO_BIN_EXE_grit");
    let dir = tempfile::TempDir::new().expect("tempdir");
    let root = dir.path();
    assert!(run_grit(grit, root, &["init"]).status.success());
    fs::write(root.join("a.txt"), b"one\n").expect("write");
    assert!(run_grit(grit, root, &["add", "a.txt"]).status.success());
    assert!(run_grit(grit, root, &["commit", "-m", "init"])
        .status
        .success());

    let patch_path = root.join("p.diff");
    fs::write(
        &patch_path,
        b"diff --git a/a.txt b/a.txt\n--- a/a.txt\n+++ b/a.txt\n@@ -1 +1 @@\n-one\n+two\n",
    )
    .expect("patch");

    let before = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-s", "a.txt"])
        .output()
        .expect("ls-files");
    assert!(before.status.success());

    let dry = run_grit(
        grit,
        root,
        &[
            "add",
            "--patch-file",
            patch_path.to_str().expect("utf8"),
            "--dry-run",
        ],
    );
    assert!(dry.status.success());

    let after = Command::new("git")
        .current_dir(root)
        .args(["ls-files", "-s", "a.txt"])
        .output()
        .expect("ls-files");
    assert!(after.status.success());
    assert_eq!(before.stdout, after.stdout);
}
