//! Issue #1034: commit refuses when no user.name/user.email (or GIT_* email) is configured.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_EMAIL")
        .env_remove("GIT_AUTHOR_NAME")
        .env_remove("GIT_COMMITTER_NAME")
        .env_remove("EMAIL")
        .output()
        .expect("run grit")
}

#[test]
fn commit_fails_without_configured_identity() -> TestResult {
    let dir = tempfile::TempDir::new()?;
    let root = dir.path();
    assert!(run_grit(root, &["init"]).status.success());
    fs::write(root.join("a.txt"), b"hi\n")?;

    let out = run_grit(root, &["commit", "-m", "first"]);
    assert!(!out.status.success(), "commit should fail: {:?}", out);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("identity unknown"),
        "expected identity hint: {stderr}"
    );
    assert!(
        stderr.contains("unable to auto-detect email address"),
        "expected email hint: {stderr}"
    );
    assert!(
        stderr.contains("grit config --global user.email"),
        "expected setup hint: {stderr}"
    );

    let json = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(root)
        .args(["commit", "-m", "first", "--json"])
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .env_remove("GIT_AUTHOR_EMAIL")
        .env_remove("GIT_COMMITTER_EMAIL")
        .output()?;
    assert!(!json.status.success());

    Ok(())
}
