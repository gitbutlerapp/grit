//! `grit remote rename` JSON output.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn isolated(mut cmd: Command, dir: &Path) -> Output {
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

fn grit(dir: &Path, args: &[&str]) -> Output {
    let mut cmd = Command::new(GRIT);
    cmd.args(args);
    isolated(cmd, dir)
}

#[test]
fn remote_rename_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let wt = tmp.path().join("upstream-wt");
    let repo = tmp.path().join("r");
    std::fs::create_dir_all(&wt).expect("mkdir");
    std::fs::create_dir_all(&repo).expect("mkdir");
    git(&wt, &["init", "-q", "-b", "main"]);
    git(&wt, &["config", "user.email", "t@example.com"]);
    git(&wt, &["config", "user.name", "Test"]);
    git(&wt, &["commit", "-q", "--allow-empty", "-m", "one"]);
    let bare = tmp.path().join("upstream.git");
    git(&wt, &["clone", "-q", "--bare", ".", bare.to_str().unwrap()]);
    git(&repo, &["init", "-q", "-b", "main"]);
    git(&repo, &["config", "user.email", "t@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["commit", "-q", "--allow-empty", "-m", "one"]);
    git(
        &repo,
        &["remote", "add", "origin", bare.to_str().unwrap()],
    );
    git(&repo, &["fetch", "-q", "origin"]);

    let out = grit(&repo, &["remote", "rename", "origin", "upstream", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("json");
    assert_eq!(json["action"], "rename");
    assert_eq!(json["old"], "origin");
    assert_eq!(json["new"], "upstream");
    assert!(json["refs_renamed"].as_u64().unwrap_or(0) >= 1);
    assert_eq!(json["dry_run"], false);
    assert!(git(&repo, &["remote"]).trim().contains("upstream"));
}
