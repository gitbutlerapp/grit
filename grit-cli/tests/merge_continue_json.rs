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
    assert!(v["conflicts"].as_array().unwrap().iter().any(|p| p == "f"));

    fs::write(repo.join("f"), "resolved\n").unwrap();
    system_git_ok(repo, &["add", "f"]);

    let (code, v) = grit_json(repo, &["merge", "--continue"]);
    assert_eq!(code, 0);
    assert_eq!(v["result"], "merged");
    assert!(v["oid"].as_str().is_some());
    assert!(!repo.join(".git/MERGE_HEAD").exists());
}
