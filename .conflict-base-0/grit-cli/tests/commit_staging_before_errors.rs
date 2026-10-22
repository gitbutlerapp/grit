//! Commit must not mutate the index when message validation fails.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn dump(&self) -> String {
        format!(
            "exit={:?}\n--- stdout ---\n{}\n--- stderr ---\n{}",
            self.status, self.stdout, self.stderr
        )
    }
}

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(tag: &str) -> Result<Self, Box<dyn Error>> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::SeqCst);
        let mut path = std::env::temp_dir();
        path.push(format!(
            "grit-cli-commit-stage-{tag}-{}-{n}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn gs<I, S>(dir: &Path, args: I) -> CmdOutput
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let out = Command::new(GS)
        .args(&args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

#[test]
fn missing_commit_message_does_not_stage_worktree_changes() -> TestResult {
    let scratch = Scratch::new("msg")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("tracked.txt"), b"base\n")?;
    git(root, &["add", "tracked.txt"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("tracked.txt"), b"dirty\n")?;
    fs::write(root.join("new.txt"), b"new\n")?;

    let out = gs(root, ["commit"]);
    assert_ne!(out.status, Some(0), "{}", out.dump());

    let porcelain = git(root, &["diff", "--cached", "--name-only"]);
    assert!(
        porcelain.trim().is_empty(),
        "failed commit must not stage anything: {porcelain:?}"
    );
    Ok(())
}

#[test]
fn missing_message_preserves_curated_index() -> TestResult {
    let scratch = Scratch::new("curated")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), b"base\n")?;
    fs::write(root.join("b"), b"base\n")?;
    git(root, &["add", "a", "b"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("a"), b"a2\n")?;
    fs::write(root.join("b"), b"b2\n")?;
    git(root, &["add", "a"]);

    let cached = git(root, &["diff", "--cached", "--name-only"]);
    assert_eq!(cached.trim(), "a", "only a should be staged");
    let unstaged = git(root, &["diff", "--name-only"]);
    assert_eq!(unstaged.trim(), "b", "only b should be unstaged");
    let before = git(root, &["status", "--short"]);

    let out = gs(root, ["commit"]);
    assert_ne!(out.status, Some(0), "{}", out.dump());

    let after = git(root, &["status", "--short"]);
    assert_eq!(
        before.trim(),
        after.trim(),
        "index must be unchanged after missing-message error"
    );
    Ok(())
}

#[test]
fn missing_message_with_staged_flag_preserves_curated_index() -> TestResult {
    let scratch = Scratch::new("staged-flag")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), b"base\n")?;
    fs::write(root.join("b"), b"base\n")?;
    git(root, &["add", "a", "b"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("a"), b"a2\n")?;
    fs::write(root.join("b"), b"b2\n")?;
    git(root, &["add", "a"]);

    let before = git(root, &["status", "--short"]);

    let out = gs(root, ["commit", "--staged"]);
    assert_ne!(out.status, Some(0), "{}", out.dump());

    let after = git(root, &["status", "--short"]);
    assert_eq!(before.trim(), after.trim());
    Ok(())
}

#[test]
fn pre_commit_hook_sees_staged_index_from_implicit_add() -> TestResult {
    let scratch = Scratch::new("hook")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a.txt"), b"base\n")?;
    git(root, &["add", "a.txt"]);
    git(root, &["commit", "-qm", "base"]);

    fs::write(root.join("a.txt"), b"dirty\n")?;

    let hook = root.join(".git/hooks/pre-commit");
    fs::write(
        &hook,
        b"#!/bin/sh\nset -e\ngit diff --cached --quiet -- a.txt && exit 1\nexit 0\n",
    )?;
    fs::set_permissions(&hook, fs::Permissions::from_mode(0o755))?;

    let out = gs(root, ["commit", "-m", "ok"]);
    assert_eq!(
        out.status,
        Some(0),
        "pre-commit must observe staged a.txt: {}",
        out.dump()
    );
    Ok(())
}
