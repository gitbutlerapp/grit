//! `grit commit --amend` without `-m` reuses the amended commit's message.

use std::error::Error;
use std::fs;
use std::process::Command;

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

fn git_log_subject(dir: &std::path::Path) -> String {
    let null = null_config();
    let out = Command::new("git")
        .current_dir(dir)
        .args(["log", "-1", "--format=%s"])
        .env("GIT_CONFIG_GLOBAL", &null)
        .env("GIT_CONFIG_SYSTEM", &null)
        .output()
        .expect("git log");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

#[test]
fn amend_without_message_preserves_subject() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["add", "."]).status.success());
    assert!(run_grit(dir.path(), &["commit", "-m", "keep this subject"])
        .status
        .success());

    fs::write(dir.path().join("a.txt"), b"a\nb\n")?;
    let amend = run_grit(dir.path(), &["commit", "--amend"]);
    assert!(
        amend.status.success(),
        "amend without -m failed: {}",
        String::from_utf8_lossy(&amend.stderr)
    );
    assert_eq!(git_log_subject(dir.path()), "keep this subject");
    Ok(())
}

#[test]
fn amend_without_message_ignores_squash_msg() -> TestResult {
    let dir = tempfile::tempdir()?;
    assert!(run_grit(dir.path(), &["init"]).status.success());
    fs::write(dir.path().join("a.txt"), b"a\n")?;
    assert!(run_grit(dir.path(), &["add", "."]).status.success());
    assert!(run_grit(dir.path(), &["commit", "-m", "tip subject"])
        .status
        .success());

    fs::write(
        dir.path().join(".git/SQUASH_MSG"),
        "Squashed commit of the following:\n",
    )?;
    fs::write(dir.path().join("a.txt"), b"a\nchanged\n")?;
    assert!(run_grit(dir.path(), &["add", "a.txt"]).status.success());

    let amend = run_grit(dir.path(), &["commit", "--amend"]);
    assert!(
        amend.status.success(),
        "amend with SQUASH_MSG present failed: {}",
        String::from_utf8_lossy(&amend.stderr)
    );
    assert_eq!(git_log_subject(dir.path()), "tip subject");
    Ok(())
}
