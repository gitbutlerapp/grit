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
    system_git_ok(repo, &["config", "rerere.enabled", "true"]);
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
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|e| e.contains("unmerged") || e.contains("f")),
        "expected continue to refuse unresolved merge: {v}"
    );

    fs::write(repo.join("f"), "resolved\n").unwrap();

    let (code, v) = grit_json(repo, &["merge", "--continue"]);
    assert_eq!(code, 0);
    assert_eq!(v["result"], "merged");
    assert_eq!(v["branch"], "side");
    assert!(v["oid"].as_str().is_some());
    assert!(!repo.join(".git/MERGE_HEAD").exists());
    let rerere = v["rerere"]
        .as_array()
        .expect("rerere array on merged continue");
    assert!(
        rerere.iter().any(|e| e["kind"] == "recorded_resolution"),
        "expected recorded_resolution in JSON: {v}"
    );
}

#[test]
fn merge_continue_rerere_human_and_markdown() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = tmp.path();
    system_git_ok(repo, &["init", "-q", "-b", "main"]);
    system_git_ok(repo, &["config", "user.name", "T"]);
    system_git_ok(repo, &["config", "user.email", "t@e.com"]);
    system_git_ok(repo, &["config", "rerere.enabled", "true"]);
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
    fs::write(repo.join("f"), "resolved\n").unwrap();

    let human = Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(["merge", "--continue"])
        .current_dir(repo)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    assert!(human.status.success());
    let human_text = format!(
        "{}{}",
        String::from_utf8_lossy(&human.stdout),
        String::from_utf8_lossy(&human.stderr)
    );
    assert!(
        human_text.contains("Recorded rerere resolution"),
        "human output should mention rerere: {human_text}"
    );

    let tmp2 = tempfile::tempdir().expect("tempdir2");
    let repo2 = tmp2.path();
    system_git_ok(repo2, &["init", "-q", "-b", "main"]);
    system_git_ok(repo2, &["config", "user.name", "T"]);
    system_git_ok(repo2, &["config", "user.email", "t@e.com"]);
    system_git_ok(repo2, &["config", "rerere.enabled", "true"]);
    fs::write(repo2.join("f"), "base\n").unwrap();
    system_git_ok(repo2, &["add", "f"]);
    system_git_ok(repo2, &["commit", "-qm", "base"]);
    system_git_ok(repo2, &["checkout", "-qb", "side"]);
    fs::write(repo2.join("f"), "side\n").unwrap();
    system_git_ok(repo2, &["commit", "-qam", "side"]);
    system_git_ok(repo2, &["checkout", "-q", "main"]);
    fs::write(repo2.join("f"), "main\n").unwrap();
    system_git_ok(repo2, &["commit", "-qam", "main"]);
    let merge2 = Command::new("git")
        .args(["merge", "side"])
        .current_dir(repo2)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("merge");
    assert_ne!(merge2.status.code(), Some(0));
    fs::write(repo2.join("f"), "resolved\n").unwrap();

    let md = Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(["--markdown", "merge", "--continue"])
        .current_dir(repo2)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("grit");
    assert!(md.status.success());
    let md_text = String::from_utf8_lossy(&md.stdout);
    assert!(
        md_text.contains("recorded_resolution") || md_text.contains("Recorded rerere"),
        "markdown should mention rerere: {md_text}"
    );
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
