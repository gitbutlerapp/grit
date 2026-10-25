//! CLI messages and JSON for commit with nothing staged (issue #1976).

use std::error::Error;
use std::fs;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn null_config() -> String {
    if cfg!(windows) {
        "NUL".to_string()
    } else {
        "/dev/null".to_string()
    }
}

fn run_grit(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    let null = null_config();
    Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("grit")
}

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let null = null_config();
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.x")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.x")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn commit_staged_with_unstaged_only_reports_nothing_staged() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    assert!(run_grit(root, &["init"]).status.success());
    fs::write(root.join("a.txt"), "1\n")?;
    git_in(root, &["add", "a.txt"]);
    git_in(root, &["commit", "-q", "-m", "one"]);
    fs::write(root.join("a.txt"), "1\n2\n")?;

    let out = run_grit(root, &["commit", "--staged", "-m", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nothing staged to commit"), "{stderr}");
    assert!(stderr.contains("a.txt"), "{stderr}");
    assert!(
        !stderr.contains("working tree clean"),
        "must not claim clean tree: {stderr}"
    );

    let out = run_grit(root, &["commit", "--staged", "-m", "nope", "--json"]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(
        v.get("kind").and_then(Value::as_str),
        Some("nothing_staged")
    );
    assert_eq!(
        v.get("nothing_to_commit")
            .and_then(|o| o.get("reason"))
            .and_then(Value::as_str),
        Some("uncommitted")
    );
    Ok(())
}

#[test]
fn commit_auto_stage_false_same_message() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    assert!(run_grit(root, &["init"]).status.success());
    fs::write(root.join("a.txt"), "1\n")?;
    git_in(root, &["add", "a.txt"]);
    git_in(root, &["commit", "-q", "-m", "one"]);
    fs::write(root.join("a.txt"), "1\n2\n")?;
    git_in(root, &["config", "commit.autoStage", "false"]);

    let out = run_grit(root, &["commit", "-m", "nope"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("nothing staged to commit"), "{stderr}");
    assert!(!stderr.contains("working tree clean"), "{stderr}");
    Ok(())
}
