//! `grit remote add` / `grit remote` / remote resolution errors, checked against `git`.

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
fn remote_add_in_linked_worktree_updates_shared_config() {
    let tmp = tempfile::tempdir().unwrap();
    let main = tmp.path().join("main");
    init_repo(&main);
    let wt = tmp.path().join("wt");
    git(&main, &["worktree", "add", "-q", wt.to_str().unwrap()]);

    let out = grit(&wt, &["remote", "add", "shared", "/srv/shared.git"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    assert_eq!(
        git(&main, &["remote", "get-url", "shared"]).trim(),
        "/srv/shared.git"
    );
    assert!(!main.join(".git/worktrees/wt/config").exists());

    let listed = grit(&main, &["remote", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&listed.stdout).unwrap();
    assert_eq!(json["remotes"][0]["name"], "shared");
    assert_eq!(json["remotes"][0]["url"], "/srv/shared.git");
}

#[test]
fn remote_list_includes_url_less_remotes_like_git() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    git(tmp.path(), &["remote", "add", "origin", "/srv/origin.git"]);
    git(
        tmp.path(),
        &[
            "config",
            "remote.half.fetch",
            "+refs/heads/*:refs/remotes/half/*",
        ],
    );

    let out = grit(tmp.path(), &["remote", "--json"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let names: Vec<&str> = json["remotes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["name"].as_str().unwrap())
        .collect();
    let git_names = git(tmp.path(), &["remote"]);
    assert_eq!(names, git_names.lines().collect::<Vec<_>>());
    assert_eq!(json["remotes"][0]["url"], "(no url)");
}

#[test]
fn remote_add_rejects_duplicate_and_invalid_names() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    assert!(grit(tmp.path(), &["remote", "add", "origin", "/srv/a.git"])
        .status
        .success());

    let dup = grit(tmp.path(), &["remote", "add", "origin", "/srv/b.git"]);
    assert!(!dup.status.success());
    assert!(String::from_utf8_lossy(&dup.stderr).contains("remote 'origin' already exists"));

    let bad = grit(tmp.path(), &["remote", "add", "bad name", "/srv/b.git"]);
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("not a valid remote name"));

    assert_eq!(
        git(tmp.path(), &["remote", "get-url", "origin"]).trim(),
        "/srv/a.git"
    );
    assert_eq!(git(tmp.path(), &["remote"]).trim(), "origin");
}

#[test]
fn unknown_remote_is_reported_by_fetch_and_remote_refs() {
    let tmp = tempfile::tempdir().unwrap();
    init_repo(tmp.path());
    for args in [&["fetch", "nosuch"][..], &["remote", "refs", "nosuch"][..]] {
        let out = grit(tmp.path(), args);
        assert!(!out.status.success(), "{args:?}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("unknown remote 'nosuch'") && stderr.contains("grit remote add"),
            "{args:?}: {stderr}"
        );
    }
}
