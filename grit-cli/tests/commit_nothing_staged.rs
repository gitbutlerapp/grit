//! `commit` JSON kind for nothing staged vs clean tree (issue #1969).

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

fn grit(dir: &std::path::Path, args: &[&str]) -> (i32, String) {
    let null = null_config();
    let out = Command::new(GRIT)
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
    )
}

fn init_repo() -> tempfile::TempDir {
    let tmp = tempfile::tempdir().expect("tempdir");
    let out = Command::new(GRIT)
        .current_dir(tmp.path())
        .arg("init")
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("init");
    assert!(out.status.success(), "init: {}", String::from_utf8_lossy(&out.stderr));
    tmp
}

#[test]
fn commit_json_nothing_staged_kind() -> TestResult {
    let root = init_repo();
    fs::write(root.path().join("a.txt"), "a\n")?;
    let (code, _) = grit(root.path(), &["commit", "-m", "base", "--json"]);
    assert_eq!(code, 0);

    grit(root.path(), &["config", "commit.autoStage", "false"]);
    fs::write(root.path().join("a.txt"), "a\nb\n")?;
    fs::write(root.path().join("n.txt"), "n\n")?;

    let (code, stdout) = grit(root.path(), &["commit", "-m", "x", "--json"]);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(v["kind"].as_str(), Some("nothing_staged"));
    assert!(
        v["error"]
            .as_str()
            .is_some_and(|e| e.contains("nothing staged to commit")),
        "{v}"
    );
    assert!(
        v["hint"].as_str().is_some_and(|h| h.contains("grit add")),
        "{v}"
    );

    let (code, stdout) = grit(root.path(), &["commit", "--staged", "-m", "x", "--json"]);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(v["kind"].as_str(), Some("nothing_staged"));
    Ok(())
}

#[test]
fn commit_json_clean_tree_kind() -> TestResult {
    let root = init_repo();
    fs::write(root.path().join("a.txt"), "a\n")?;
    let (code, _) = grit(root.path(), &["commit", "-m", "base", "--json"]);
    assert_eq!(code, 0);
    grit(root.path(), &["config", "commit.autoStage", "false"]);
    let (code, stdout) = grit(root.path(), &["commit", "-m", "x", "--json"]);
    assert_eq!(code, 1);
    let v: Value = serde_json::from_str(stdout.trim())?;
    assert_eq!(v["kind"].as_str(), Some("nothing_to_commit"));
    Ok(())
}
