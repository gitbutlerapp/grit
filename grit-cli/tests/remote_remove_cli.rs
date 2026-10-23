//! `grit remote remove` JSON, dry-run, and unknown-remote exit code.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn isolated(mut cmd: Command, dir: &Path) -> std::process::Output {
    cmd.current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .expect("spawn")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut cmd = Command::new("git");
    cmd.args(args);
    let out = isolated(cmd, dir);
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout).unwrap()
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    let mut cmd = Command::new(GRIT);
    cmd.args(args);
    isolated(cmd, dir)
}

fn init_repo(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(
        dir,
        &[
            "-c",
            "user.name=T",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-q",
            "--allow-empty",
            "-m",
            "one",
        ],
    );
    git(dir, &["remote", "add", "origin", "/tmp/x.git"]);
    let head = {
        let mut cmd = Command::new("git");
        cmd.args(["rev-parse", "HEAD"]).current_dir(dir);
        let out = isolated(cmd, dir);
        String::from_utf8(out.stdout).unwrap().trim().to_owned()
    };
    git(
        dir,
        &[
            "update-ref",
            "refs/remotes/origin/main",
            &head,
        ],
    );
}

#[test]
fn remote_remove_json_fields_and_dry_run() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());

    let dry = grit(
        tmp.path(),
        &["remote", "remove", "origin", "--dry-run", "--json"],
    );
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&dry.stdout).unwrap();
    assert_eq!(json["action"], "remove");
    assert_eq!(json["name"], "origin");
    assert_eq!(json["dry_run"], true);
    assert!(json["refs_deleted"].is_array());
    assert!(json["branches_untracked"].is_array());
    assert_eq!(
        git(tmp.path(), &["remote"]).trim(),
        "origin",
        "dry-run must not remove remote"
    );

    let out = grit(tmp.path(), &["remote", "remove", "origin", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["action"], "remove");
    assert_eq!(json["dry_run"], false);
    assert!(json["refs_deleted"]
        .as_array()
        .unwrap()
        .iter()
        .any(|v| v.as_str() == Some("refs/remotes/origin/main")));
    assert!(git(tmp.path(), &["remote"]).trim().is_empty());
}

#[test]
fn remote_remove_unknown_remote_exits_2() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    let out = grit(tmp.path(), &["remote", "remove", "nosuch"]);
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown remote 'nosuch'"));
}
