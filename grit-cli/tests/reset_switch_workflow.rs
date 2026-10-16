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
fn reset_and_revert_peel_annotated_tag() -> TestResult {
    let scratch = Scratch::new("reset-tag-peel")?;
    let repo = scratch.path();
    gs_ok(repo, ["init"])?;
    git(repo, &["config", "user.name", "T"])?;
    git(repo, &["config", "user.email", "t@e.com"])?;
    fs::write(repo.join("a.txt"), "a\n")?;
    gs_ok(repo, ["commit", "-m", "one"])?;
    fs::write(repo.join("a.txt"), "b\n")?;
    gs_ok(repo, ["commit", "-m", "two"])?;

    let parent = Command::new("git")
        .args(["rev-parse", "HEAD~1"])
        .current_dir(repo)
        .output()?;
    if !parent.status.success() {
        return Err(format!(
            "expected two commits before tagging: {}",
            String::from_utf8_lossy(&parent.stderr)
        )
        .into());
    }
    let parent_oid = String::from_utf8_lossy(&parent.stdout).trim().to_owned();
    gs_ok(
        repo,
        ["tag", "v1", "-m", "release", "--object", &parent_oid],
    )?;
    gs_ok(repo, ["reset", "--hard", "v1"])?;
    gs_ok(repo, ["reset", "--soft", "v1"])?;
    gs_ok(repo, ["revert", "v1"])?;

    Ok(())
}

#[test]
fn reset_and_revert_reject_tree_ish_without_panic() -> TestResult {
    let scratch = Scratch::new("reset-tree-reject")?;
    let repo = scratch.path();
    gs_ok(repo, ["init"])?;
    git(repo, &["config", "user.name", "T"])?;
    git(repo, &["config", "user.email", "t@e.com"])?;
    fs::write(repo.join("a.txt"), "a\n")?;
    git(repo, &["add", "-A"])?;
    git(repo, &["commit", "-qm", "one"])?;

    for args in [
        &["reset", "--hard", "HEAD^{tree}"][..],
        &["revert", "HEAD^{tree}"][..],
    ] {
        let out = gs(repo, args);
        assert!(!out.status.success(), "expected failure for {args:?}");
        assert_ne!(
            out.status.code(),
            Some(101),
            "must not panic (exit 101): stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(stderr.contains("does not name a commit"), "stderr={stderr}");
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
