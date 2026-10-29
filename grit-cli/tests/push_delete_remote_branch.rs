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
    assert_eq!(json["results"][0]["deleted"].as_bool(), Some(true));
}

fn assert_delete_human_stdout(stdout: &str) {
    assert!(
        stdout.contains("deleted"),
        "stdout should mention deletion: {stdout}"
    );
    assert!(
        !stdout.contains("pushed"),
        "stdout should not look like a normal push: {stdout}"
    );
}

#[test]
fn delete_remote_branch_human_output_says_deleted() {
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
    git(&local, &["branch", "tmpdel"]);
    git(&local, &["push", "-q", "origin", "HEAD:refs/heads/tmpdel"]);
    git(&local, &["checkout", "main"]);
    git(&local, &["branch", "-D", "tmpdel"]);

    let out = grit(
        &local,
        &["push", "--delete", "tmpdel", "--remote", "origin"],
    );
    assert!(
        out.status.success(),
        "grit push --delete: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert_delete_human_stdout(&stdout);
    assert!(stdout.contains("deleted tmpdel from origin"), "{stdout}");

    git(&local, &["push", "-q", "origin", "main:refs/heads/tmpdel"]);
    let out = grit(
        &local,
        &[
            "push",
            "--delete",
            "tmpdel",
            "--remote",
            "origin",
            "--dry-run",
        ],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("would delete tmpdel from origin"),
        "{stdout}"
    );
    assert!(!stdout.contains("pushed"), "{stdout}");
}

#[test]
fn delete_remote_tag_human_and_json() {
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
    git(&local, &["tag", "v-del"]);
    git(&local, &["push", "-q", "origin", "refs/tags/v-del"]);

    let out = grit(&local, &["push", "--delete", "v-del", "--remote", "origin"]);
    assert!(out.status.success());
    assert_delete_human_stdout(&String::from_utf8_lossy(&out.stdout));

    git(&local, &["push", "-q", "origin", "refs/tags/v-del"]);
    let out = grit(
        &local,
        &["push", "--delete", "v-del", "--remote", "origin", "--json"],
    );
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(json["kind"].as_str(), Some("delete"));
    assert_eq!(json["results"][0]["deleted"].as_bool(), Some(true));
}

#[test]
fn colon_refspec_delete_uses_deleted_output() {
    let tmp = tempfile::tempdir().unwrap();
    let (origin, local) = remote_with_branches(tmp.path());

    let out = grit(&local, &["push", "origin", ":a"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_delete_human_stdout(&String::from_utf8_lossy(&out.stdout));
    assert!(
        !ls_remote_heads(&origin).contains(&"refs/heads/a".to_owned()),
        "branch a should be gone"
    );

    let out = grit(&local, &["push", "origin", ":b", "--json"]);
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(json["results"][0]["deleted"].as_bool(), Some(true));
}

/// A work tree whose `origin` bare remote has `main`, `a`, `b` and `c`.
fn remote_with_branches(root: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
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
    git(
        &local,
        &["push", "-q", "origin", "main", "main:a", "main:b", "main:c"],
    );
    (origin, local)
}

fn grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit")
}

#[test]
fn delete_two_remote_branches_in_one_push() {
    let tmp = tempfile::tempdir().unwrap();
    let (origin, local) = remote_with_branches(tmp.path());

    let out = grit(&local, &["push", "origin", "--delete", "a", "b", "--json"]);
    assert!(
        out.status.success(),
        "grit push origin --delete a b: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(json["kind"].as_str(), Some("delete"));
    assert!(json.get("branch").is_none(), "{json}");
    let refs: Vec<_> = json["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["ref"].as_str().unwrap(), r["status"].as_str().unwrap()))
        .collect();
    assert_eq!(refs, [("refs/heads/a", "ok"), ("refs/heads/b", "ok")]);
    for row in json["results"].as_array().unwrap() {
        assert_eq!(row["deleted"].as_bool(), Some(true));
    }
    assert_eq!(
        ls_remote_heads(&origin),
        ["refs/heads/c", "refs/heads/main"]
    );

    let out = grit(&local, &["push", "--delete", "origin", "c"]);
    assert!(out.status.success());
    assert_eq!(ls_remote_heads(&origin), ["refs/heads/main"]);
}

#[test]
fn delete_reports_a_name_missing_on_the_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let (origin, local) = remote_with_branches(tmp.path());
    git(&local, &["branch", "newbr"]);

    let out = grit(
        &local,
        &["push", "origin", "--delete", "c", "newbr", "--json"],
    );
    assert_eq!(out.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert_eq!(json["results"][0]["status"].as_str(), Some("ok"));
    assert_eq!(json["results"][1]["ref"].as_str(), Some("refs/heads/newbr"));
    assert_eq!(
        json["results"][1]["reason"].as_str(),
        Some("remote_ref_missing")
    );
    assert!(!ls_remote_heads(&origin).contains(&"refs/heads/c".to_owned()));
}

#[test]
fn delete_with_a_refspec_is_rejected_before_touching_the_remote() {
    let tmp = tempfile::tempdir().unwrap();
    let (origin, local) = remote_with_branches(tmp.path());
    let before = ls_remote_heads(&origin);

    let out = grit(
        &local,
        &["push", "origin", "HEAD:refs/heads/release", "--delete", "b"],
    );
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("HEAD:refs/heads/release"),
        "error must name the refspec: {stderr}"
    );
    assert_eq!(ls_remote_heads(&origin), before);

    let out = grit(
        &local,
        &[
            "push",
            "origin",
            "--delete",
            "b",
            "HEAD:refs/heads/release",
            "--json",
        ],
    );
    assert!(!out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON");
    assert!(
        json["error"]
            .as_str()
            .is_some_and(|e| e.contains("HEAD:refs/heads/release")),
        "{json}"
    );
    assert_eq!(ls_remote_heads(&origin), before);
}

#[test]
fn delete_without_refs_is_an_error() {
    let tmp = tempfile::tempdir().unwrap();
    let (origin, local) = remote_with_branches(tmp.path());
    let before = ls_remote_heads(&origin);
    for args in [&["push", "--delete"][..], &["push", "--delete", "origin"]] {
        let out = grit(&local, args);
        assert!(!out.status.success(), "{args:?} must fail");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("--delete"),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(ls_remote_heads(&origin), before);
}
