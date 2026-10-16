//! Workflows for `grit reset` and extended `grit switch` (issue #952).

use std::error::Error;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!("grit-cli-{tag}-{}-{n}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

fn gs(repo: &Path, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> std::process::Output {
    Command::new(GS)
        .args(args)
        .current_dir(repo)
        .output()
        .expect("spawn grit")
}

fn gs_ok(
    repo: &Path,
    args: impl IntoIterator<Item = impl AsRef<OsStr>>,
) -> Result<(), Box<dyn Error>> {
    let out = gs(repo, args);
    if !out.status.success() {
        return Err(format!(
            "grit failed: status={:?}\nstdout={}\nstderr={}",
            out.status.code(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

fn git(repo: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git").args(args).current_dir(repo).output()?;
    if !out.status.success() {
        return Err(format!("git failed: {:?}", out.status.code()).into());
    }
    Ok(())
}

#[test]
fn reset_soft_uncommit() -> TestResult {
    let scratch = Scratch::new("reset-soft")?;
    let repo = scratch.path();
    gs_ok(repo, ["init"])?;
    git(repo, &["config", "user.name", "a"])?;
    git(repo, &["config", "user.email", "a@e.invalid"])?;
    fs::write(repo.join("f"), "1\n")?;
    gs_ok(repo, ["commit", "-m", "c1"])?;
    fs::write(repo.join("f"), "2\n")?;
    gs_ok(repo, ["commit", "-m", "c2"])?;

    gs_ok(repo, ["reset", "--soft", "HEAD~1"])?;
    let out = gs(repo, ["status", "--json"]);
    let json: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    assert!(json["staged"].as_array().is_some_and(|a| !a.is_empty()));

    Ok(())
}

#[test]
fn switch_detach_and_dirty_unrelated() -> TestResult {
    let scratch = Scratch::new("switch-detach-dirty")?;
    let repo = scratch.path();
    gs_ok(repo, ["init"])?;
    git(repo, &["config", "user.name", "a"])?;
    git(repo, &["config", "user.email", "a@e.invalid"])?;
    fs::write(repo.join("f"), "1\n")?;
    gs_ok(repo, ["commit", "-m", "c1"])?;
    fs::write(repo.join("f"), "2\n")?;
    gs_ok(repo, ["commit", "-m", "c2"])?;
    gs_ok(repo, ["switch", "-c", "other"])?;
    gs_ok(repo, ["switch", "main"])?;
    fs::write(repo.join("unrelated"), "dirty\n")?;

    gs_ok(repo, ["switch", "other"])?;
    assert!(repo.join("unrelated").exists());

    gs_ok(repo, ["switch", "--detach", "HEAD~1"])?;
    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()?;
    assert_eq!(head.stdout.len(), 41);

    Ok(())
}
