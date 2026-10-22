//! `grit merge --continue` JSON contract.

use std::fs;
use std::path::Path;
use std::process::Command;

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn system_git_ok(repo: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} failed", args);
}

fn grit_json(repo: &Path, args: &[&str]) -> (i32, serde_json::Value) {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(args)
        .args(["--json"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    let code = out.status.code().unwrap_or(-1);
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).unwrap_or(serde_json::Value::Null);
    (code, value)
}

#[test]
fn merge_continue_json() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f"), "base\n").unwrap();
    system_git_ok(repo, &["add", "f"]);
    system_git_ok(repo, &["commit", "-qm", "base"]);
    system_git_ok(repo, &["checkout", "-qb", "side"]);
    fs::write(repo.join("f"), "side\n").unwrap();
    system_git_ok(repo, &["commit", "-qam", "side"]);
    system_git_ok(repo, &["checkout", "-q", "main"]);
    fs::write(repo.join("f"), "main\n").unwrap();
    system_git_ok(repo, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .args(["merge", "side"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("merge");
    assert_ne!(merge.status.code(), Some(0));

    let (code, v) = grit_json(repo, &["merge", "--continue"]);
    assert_ne!(code, 0);
    assert_eq!(v["result"], "conflicts");
    assert_eq!(v["branch"], "side");
    let conflicts = v["conflicts"].as_array().expect("conflicts array");
    assert!(
        conflicts.iter().any(|c| c["path"].as_str() == Some("f")),
        "expected conflict on f: {v}"
    );

    fs::write(repo.join("f"), "resolved\n").unwrap();
    system_git_ok(repo, &["add", "f"]);

    let (code, v) = grit_json(repo, &["merge", "--continue"]);
    assert_eq!(code, 0);
    assert_eq!(v["result"], "merged");
    assert_eq!(v["branch"], "side");
    assert!(v["oid"].as_str().is_some());
    assert!(!repo.join(".git/MERGE_HEAD").exists());
}

#[test]
fn merge_branch_with_continue_is_usage_error() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(["merge", "topic", "--continue", "--json"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    assert_eq!(
        out.status.code(),
        Some(2),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("json on stdout");
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|e| e.contains("continue") || e.contains("topic")),
        "expected usage error JSON: {v}"
    );
}
