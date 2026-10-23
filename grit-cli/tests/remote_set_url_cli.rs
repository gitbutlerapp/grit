//! `grit remote set-url` and `grit remote -v` CLI output.

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
}

#[test]
fn remote_set_url_json() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "/old.git"]);

    let out = grit(
        tmp.path(),
        &[
            "remote",
            "set-url",
            "origin",
            "https://ex.example/r.git",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["action"], "set_url");
    assert_eq!(json["name"], "origin");
    assert_eq!(json["kind"], "fetch");
    assert_eq!(json["old_url"], "/old.git");
    assert_eq!(json["url"], "https://ex.example/r.git");
    assert_eq!(json["dry_run"], false);
    assert_eq!(
        git(tmp.path(), &["remote", "get-url", "origin"]).trim(),
        "https://ex.example/r.git"
    );
}

#[test]
fn remote_verbose_json_fields() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "gh:org/repo.git"]);
    git(
        tmp.path(),
        &["config", "remote.origin.pushurl", "gh:org/push.git"],
    );
    git(
        tmp.path(),
        &["config", "url.https://github.com/.insteadOf", "gh:"],
    );

    let out = grit(tmp.path(), &["remote", "-v", "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let entry = &json["remotes"][0];
    assert_eq!(entry["name"], "origin");
    assert_eq!(entry["url"], "gh:org/repo.git");
    assert_eq!(entry["fetch_url"], "https://github.com/org/repo.git");
    assert_eq!(entry["push_url"], "https://github.com/org/push.git");
    assert_eq!(entry["refspecs"][0], "+refs/heads/*:refs/remotes/origin/*");
}

#[test]
fn remote_verbose_human_matches_git_layout() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "/fetch.git"]);
    git(
        tmp.path(),
        &["remote", "set-url", "--push", "origin", "/push.git"],
    );

    let git_v = git(tmp.path(), &["remote", "-v"]);
    let out = grit(tmp.path(), &["remote", "-v"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let grit_v = String::from_utf8(out.stdout).unwrap();
    let normalize = |s: &str| {
        s.lines()
            .map(|l| l.replace('\t', " "))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(normalize(&grit_v), normalize(&git_v));
}

#[test]
fn push_uses_set_url_push_target() {
    let tmp = tempfile::tempdir().unwrap();
    let bare_a = tmp.path().join("a.git");
    let bare_b = tmp.path().join("b.git");
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare_a.to_str().unwrap()],
    );
    git(
        tmp.path(),
        &["init", "-q", "--bare", bare_b.to_str().unwrap()],
    );
    init_repo(tmp.path());
    git(
        tmp.path(),
        &["remote", "add", "origin", bare_a.to_str().unwrap()],
    );
    let out = grit(
        tmp.path(),
        &[
            "remote",
            "set-url",
            "origin",
            bare_b.to_str().unwrap(),
            "--push",
            "--json",
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let push = grit(tmp.path(), &["push", "origin", "main", "--json"]);
    assert!(
        push.status.success(),
        "{}",
        String::from_utf8_lossy(&push.stderr)
    );
    assert_eq!(
        git(bare_b.as_path(), &["rev-parse", "main"]).trim(),
        git(tmp.path(), &["rev-parse", "main"]).trim()
    );
}
