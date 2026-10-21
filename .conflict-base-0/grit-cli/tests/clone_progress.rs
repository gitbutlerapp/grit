//! Clone progress output when stderr is not a TTY.

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

fn git_in(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let status = Command::new("git").args(args).current_dir(dir).status()?;
    assert!(
        status.success(),
        "git {:?} failed in {}",
        args,
        dir.display()
    );
    Ok(())
}

#[test]
fn clone_progress_silent_when_not_tty() -> TestResult {
    let base = tempfile::tempdir()?;
    let origin = base.path().join("origin.git");
    fs::create_dir_all(&origin)?;
    git_in(
        base.path(),
        &["init", "--bare", "-b", "main", origin.to_str().unwrap()],
    )?;
    git_in(
        base.path(),
        &["clone", "-q", origin.to_str().unwrap(), "seed"],
    )?;
    let seed = base.path().join("seed");
    fs::write(seed.join("README.md"), "hello\n")?;
    git_in(&seed, &["add", "README.md"])?;
    git_in(
        &seed,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=t@example.com",
            "commit",
            "-m",
            "init",
        ],
    )?;
    git_in(&seed, &["branch", "-M", "main"])?;
    git_in(&seed, &["push", "-u", "origin", "main"])?;

    let clone_dir = base.path().join("clone");
    let out = Command::new(GS)
        .args([
            "clone",
            origin.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ])
        .current_dir(base.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(out.status.success(), "clone failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    let stderr_lines: Vec<&str> = stderr.lines().filter(|l| !l.is_empty()).collect();
    assert_eq!(
        stderr_lines.len(),
        1,
        "expected one stderr line, got:\n{stderr}"
    );
    assert!(
        stderr_lines[0].starts_with("Cloning into ") && stderr_lines[0].contains("clone"),
        "unexpected stderr: {}",
        stderr_lines[0]
    );
    assert!(
        !stderr.contains("Receiving objects"),
        "progress must not appear without a TTY: {stderr}"
    );

    let json_dir = base.path().join("clone-json");
    let json_out = Command::new(GS)
        .args([
            "--json",
            "clone",
            origin.to_str().unwrap(),
            json_dir.to_str().unwrap(),
        ])
        .current_dir(base.path())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(json_out.status.success(), "json clone failed");
    assert!(
        json_out.stderr.is_empty(),
        "json clone stderr must be empty: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    Ok(())
}
