//! CLI flags and JSON scope for `grit commit`.

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
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("grit")
}

#[test]
fn commit_scope_flags_conflict() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["add", "."]).status.success());
    assert!(run_grit(dir.path(), &["commit", "-m", "init"])
        .status
        .success());

    let out = run_grit(dir.path(), &["commit", "-m", "x", "--staged", "--all"]);
    assert!(!out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("--staged") || stderr.contains("only one"));
    Ok(())
}

#[test]
fn commit_json_includes_scope() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["add", "."]).status.success());
    assert!(run_grit(dir.path(), &["commit", "-m", "init"])
        .status
        .success());
    fs::write(dir.path().join("a.txt"), b"b\n")?;

    let out = run_grit(
        dir.path(),
        &["commit", "-m", "paths", "--json", "--", "a.txt"],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v.get("scope").and_then(Value::as_str), Some("paths"));
    Ok(())
}

fn git_in(dir: &std::path::Path, args: &[&str]) -> String {
    let null = null_config();
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
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
fn commit_paths_with_held_index_lock_creates_nothing() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path();
    git_in(root, &["init", "-q", "-b", "main"]);
    fs::write(root.join("f1.txt"), b"a\n")?;
    git_in(root, &["add", "."]);
    git_in(root, &["commit", "-qm", "init"]);
    fs::write(root.join("f1.txt"), b"a\nb\n")?;
    let lock = root.join(".git/index.lock");
    fs::write(&lock, b"")?;
    let head = git_in(root, &["rev-parse", "HEAD"]);

    let out = run_grit(root, &["commit", "-m", "x", "--", "f1.txt"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("index.lock"), "{stderr}");
    assert!(!stderr.contains("I/O error"), "{stderr}");

    let out = run_grit(root, &["commit", "-m", "x", "--json", "--", "f1.txt"]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(v.get("kind").and_then(Value::as_str), Some("index_locked"));

    assert_eq!(git_in(root, &["rev-parse", "HEAD"]), head);
    assert!(lock.exists(), "foreign lock must be left in place");
    fs::remove_file(&lock)?;
    assert_eq!(git_in(root, &["status", "--porcelain"]), " M f1.txt\n");
    assert_eq!(
        git_in(root, &["reflog", "--format=%gs"]),
        "commit (initial): init\n"
    );

    let out = run_grit(root, &["commit", "-m", "x", "--", "f1.txt"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(git_in(root, &["status", "--porcelain"]), "");
    git_in(root, &["fsck", "--strict", "--no-progress"]);
    Ok(())
}
