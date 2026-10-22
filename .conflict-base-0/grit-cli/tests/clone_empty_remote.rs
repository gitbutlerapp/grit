//! `grit clone` succeeds for an empty bare remote (issue #991).

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()?;
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        dir.display()
    );
    Ok(())
}

#[test]
fn clone_empty_bare_exits_zero_and_creates_directory() -> TestResult {
    let base = tempfile::tempdir()?;
    let origin = base.path().join("origin.git");
    fs::create_dir_all(&origin)?;
    git_in(
        base.path(),
        &[
            "init",
            "-q",
            "--bare",
            "-b",
            "main",
            origin.to_str().unwrap(),
        ],
    )?;

    let dest = base.path().join("A");
    let out = Command::new(GRIT)
        .args(["clone", origin.to_str().unwrap(), dest.to_str().unwrap()])
        .output()?;
    assert!(
        out.status.success(),
        "grit clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(dest.is_dir());
    assert!(dest.join(".git").is_dir());

    let json = Command::new(GRIT)
        .args([
            "clone",
            "--json",
            origin.to_str().unwrap(),
            base.path().join("B").to_str().unwrap(),
        ])
        .output()?;
    assert!(
        json.status.success(),
        "{}",
        String::from_utf8_lossy(&json.stderr)
    );
    let parsed: serde_json::Value = serde_json::from_slice(&json.stdout)?;
    assert_eq!(parsed["files"], 0);
    assert_eq!(parsed["branch"], "main");
    assert_eq!(parsed["no_checkout_reason"], "empty_remote");

    Ok(())
}
