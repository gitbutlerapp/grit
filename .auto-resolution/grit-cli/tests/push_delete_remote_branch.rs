//! `grit push --delete` removes a remote branch after the local branch is gone.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

fn ls_remote_heads(origin: &Path) -> Vec<String> {
    let out = Command::new("git")
        .args(["ls-remote", "--heads", origin.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.split_once('\t').map(|(_, r)| r.to_owned()))
        .collect()
}

#[test]
fn delete_remote_branch_after_local_branch_deleted() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);

    let local = root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "x\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-qm", "seed"]);
    git(
        &local,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&local, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    git(&local, &["branch", "old-topic"]);
    std::fs::write(local.join("g"), "y\n").unwrap();
    git(&local, &["add", "g"]);
    git(&local, &["commit", "-qm", "topic"]);
    git(
        &local,
        &["push", "-q", "origin", "HEAD:refs/heads/old-topic"],
    );
    assert!(ls_remote_heads(&origin).contains(&"refs/heads/old-topic".to_owned()));

    git(&local, &["checkout", "main"]);
    git(&local, &["branch", "-D", "old-topic"]);

    let out = Command::new(GRIT)
        .args([
            "push",
            "--delete",
            "old-topic",
            "--remote",
            "origin",
            "--json",
        ])
        .current_dir(&local)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit push --delete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("push --json stdout is valid JSON");
    assert_eq!(
        json.get("branch").and_then(|v| v.as_str()),
        Some("old-topic"),
        "branch must name the deleted ref, not a flag: {json}"
    );

    let heads = ls_remote_heads(&origin);
    assert!(
        !heads.iter().any(|r| r == "refs/heads/old-topic"),
        "remote branch should be deleted, still have: {heads:?}"
    );
}

#[test]
fn delete_remote_branch_json_uses_short_name_for_qualified_ref() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let origin = root.join("origin.git");
    std::fs::create_dir_all(&origin).unwrap();
    git(&origin, &["init", "-q", "--bare", "-b", "main"]);

    let local = root.join("local");
    std::fs::create_dir_all(&local).unwrap();
    git(&local, &["init", "-q", "-b", "main"]);
    std::fs::write(local.join("f"), "x\n").unwrap();
    git(&local, &["add", "f"]);
    git(&local, &["commit", "-qm", "seed"]);
    git(
        &local,
        &["remote", "add", "origin", origin.to_str().unwrap()],
    );
    git(&local, &["push", "-q", "origin", "HEAD:refs/heads/main"]);
    git(&local, &["branch", "topic"]);
    git(&local, &["push", "-q", "origin", "HEAD:refs/heads/topic"]);
    git(&local, &["checkout", "main"]);
    git(&local, &["branch", "-D", "topic"]);

    let out = Command::new(GRIT)
        .args([
            "push",
            "--delete",
            "refs/heads/topic",
            "--remote",
            "origin",
            "--json",
        ])
        .current_dir(&local)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit push --delete refs/heads/topic: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("push --json stdout is valid JSON");
    assert_eq!(
        json.get("branch").and_then(|v| v.as_str()),
        Some("topic"),
        "branch must be short name even when --delete uses refs/heads/: {json}"
    );
    assert_eq!(json["results"][0]["ref"].as_str(), Some("refs/heads/topic"));
}
