//! Fetch and pull progress output when stderr is not a TTY.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
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

fn seed_bare_remote(base: &Path) -> Result<(PathBuf, PathBuf), Box<dyn Error>> {
    let origin = base.join("origin.git");
    fs::create_dir_all(&origin)?;
    git_in(
        base,
        &["init", "--bare", "-b", "main", origin.to_str().unwrap()],
    )?;
    git_in(base, &["clone", "-q", origin.to_str().unwrap(), "seed"])?;
    let seed = base.join("seed");
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
    Ok((origin, seed))
}

fn grit_clone_into(base: &Path, origin: &Path, dir_name: &str) -> Result<PathBuf, Box<dyn Error>> {
    let clone_dir = base.join(dir_name);
    let out = Command::new(GS)
        .args([
            "clone",
            origin.to_str().unwrap(),
            clone_dir.to_str().unwrap(),
        ])
        .current_dir(base)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(
        out.status.success(),
        "clone failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    Ok(clone_dir)
}

#[test]
fn fetch_progress_silent_when_not_tty() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, seed) = seed_bare_remote(base.path())?;
    let clone_dir = grit_clone_into(base.path(), &origin, "clone")?;

    fs::write(seed.join("README.md"), "hello world\n")?;
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
            "update",
        ],
    )?;
    git_in(&seed, &["push", "origin", "main"])?;

    let out = Command::new(GS)
        .args(["fetch"])
        .current_dir(&clone_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(out.status.success(), "fetch failed");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Receiving objects"),
        "progress must not appear without a TTY: {stderr}"
    );
    assert!(
        !stderr.contains("Indexing objects"),
        "progress must not appear without a TTY: {stderr}"
    );

    let json_out = Command::new(GS)
        .args(["--json", "fetch"])
        .current_dir(&clone_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(json_out.status.success(), "json fetch failed");
    assert!(
        json_out.stderr.is_empty(),
        "json fetch stderr must be empty: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    Ok(())
}

#[test]
fn pull_progress_silent_when_not_tty() -> TestResult {
    let base = tempfile::tempdir()?;
    let (origin, seed) = seed_bare_remote(base.path())?;
    let clone_dir = grit_clone_into(base.path(), &origin, "clone")?;

    fs::write(seed.join("README.md"), "pull me\n")?;
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
            "ahead",
        ],
    )?;
    git_in(&seed, &["push", "origin", "main"])?;

    let out = Command::new(GS)
        .args(["pull"])
        .current_dir(&clone_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(
        out.status.success(),
        "pull failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stderr.contains("Receiving objects"),
        "progress must not appear without a TTY: {stderr}"
    );
    assert!(
        !stderr.contains("Indexing objects"),
        "progress must not appear without a TTY: {stderr}"
    );

    fs::write(seed.join("README.md"), "again\n")?;
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
            "again",
        ],
    )?;
    git_in(&seed, &["push", "origin", "main"])?;

    let json_out = Command::new(GS)
        .args(["--json", "pull"])
        .current_dir(&clone_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()?;
    assert!(json_out.status.success(), "json pull failed");
    assert!(
        json_out.stderr.is_empty(),
        "json pull stderr must be empty: {}",
        String::from_utf8_lossy(&json_out.stderr)
    );
    Ok(())
}
