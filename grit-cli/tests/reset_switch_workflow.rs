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

fn parse_recovery_command_from_reset_output(stdout: &[u8]) -> Result<String, Box<dyn Error>> {
    let text = String::from_utf8_lossy(stdout);
    let marker = "Recover ";
    let start = text
        .find(marker)
        .ok_or("reset output missing orphan recovery hint")?;
    let after = &text[start + marker.len()..];
    let backtick = after
        .find('`')
        .ok_or("recovery hint missing opening backtick")?;
    let rest = &after[backtick + 1..];
    let end = rest
        .find('`')
        .ok_or("recovery hint missing closing backtick")?;
    Ok(rest[..end].to_string())
}

#[test]
fn reset_hard_orphan_recovery_hint_restores_worktree() -> TestResult {
    let scratch = Scratch::new("reset-hard-recover-hint")?;
    let repo = scratch.path();
    gs_ok(repo, ["init"])?;
    git(repo, &["config", "user.name", "T"])?;
    git(repo, &["config", "user.email", "t@e.com"])?;
    for i in 1..=3 {
        fs::write(repo.join(format!("f{i}")), format!("{i}\n"))?;
        gs_ok(repo, ["commit", "-m", &format!("c{i}")])?;
    }

    let hard_out = gs(repo, ["reset", "--hard", "HEAD~2"]);
    if !hard_out.status.success() {
        return Err(format!(
            "reset --hard failed: {}",
            String::from_utf8_lossy(&hard_out.stderr)
        )
        .into());
    }
    let recovery = parse_recovery_command_from_reset_output(&hard_out.stdout)?;
    assert!(
        recovery.contains("--hard"),
        "expected hard recovery command, got: {recovery}"
    );

    let parts: Vec<&str> = recovery.split_whitespace().collect();
    let recover_args: Vec<&str> = parts.iter().copied().skip(1).collect();
    gs_ok(repo, recover_args)?;

    for name in ["f1", "f2", "f3"] {
        assert!(
            repo.join(name).is_file(),
            "expected {name} after following recovery hint"
        );
    }
    let status = Command::new("git")
        .args(["status", "--short"])
        .current_dir(repo)
        .output()?;
    assert!(
        status.status.success(),
        "git status failed: {}",
        String::from_utf8_lossy(&status.stderr)
    );
    assert!(
        status.stdout.is_empty(),
        "worktree should be clean: {}",
        String::from_utf8_lossy(&status.stdout)
    );

    Ok(())
}

#[test]
fn reset_soft_orphan_recovery_hint_moves_head_only() -> TestResult {
    let scratch = Scratch::new("reset-soft-recover-hint")?;
    let repo = scratch.path();
    gs_ok(repo, ["init"])?;
    git(repo, &["config", "user.name", "T"])?;
    git(repo, &["config", "user.email", "t@e.com"])?;
    for i in 1..=3 {
        fs::write(repo.join(format!("f{i}")), format!("{i}\n"))?;
        gs_ok(repo, ["commit", "-m", &format!("c{i}")])?;
    }

    let soft_out = gs(repo, ["reset", "--soft", "HEAD~2"]);
    if !soft_out.status.success() {
        return Err(format!(
            "reset --soft failed: {}",
            String::from_utf8_lossy(&soft_out.stderr)
        )
        .into());
    }
    let recovery = parse_recovery_command_from_reset_output(&soft_out.stdout)?;
    assert!(
        recovery.contains("--soft"),
        "expected soft recovery command, got: {recovery}"
    );

    let orig_head = Command::new("git")
        .args(["rev-parse", "ORIG_HEAD"])
        .current_dir(repo)
        .output()?;
    let orig_head = String::from_utf8_lossy(&orig_head.stdout).trim().to_owned();

    let parts: Vec<&str> = recovery.split_whitespace().collect();
    let recover_args: Vec<&str> = parts.iter().copied().skip(1).collect();
    gs_ok(repo, recover_args)?;

    let head = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(repo)
        .output()?;
    assert_eq!(
        String::from_utf8_lossy(&head.stdout).trim(),
        orig_head.as_str(),
        "soft recovery should restore the previous tip"
    );
    for name in ["f1", "f2", "f3"] {
        assert!(
            repo.join(name).is_file(),
            "expected {name} after soft recovery"
        );
    }
    let status = Command::new("git")
        .args(["status", "--short"])
        .current_dir(repo)
        .output()?;
    assert!(
        status.stdout.is_empty(),
        "worktree should be clean after soft recovery: {}",
        String::from_utf8_lossy(&status.stdout)
    );

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
