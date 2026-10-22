//! Shared `--empty` and `--keep-committer` on history subcommands.

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

#[derive(Debug)]
struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl CmdOutput {
    fn success(&self) -> bool {
        self.status == Some(0)
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
            "grit-cli-history-shared-{tag}-{}-{n}",
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

fn null_device() -> OsString {
    if cfg!(windows) {
        OsString::from("NUL")
    } else {
        OsString::from("/dev/null")
    }
}

fn gs<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
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
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn init_undo_redo_repo(dir: &Path) -> Result<(String, String, String), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("f.txt"), "a\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "a"])?;
    let a = git(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    fs::write(dir.join("f.txt"), "b\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "b"])?;
    let b = git(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    fs::write(dir.join("f.txt"), "a\n")?;
    git(dir, &["add", "f.txt"])?;
    git(dir, &["commit", "-m", "c"])?;
    let c = git(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    Ok((a, b, c))
}

fn committer_line_from_object(dir: &Path, oid: &str) -> Result<String, Box<dyn Error>> {
    let raw = git(dir, &["cat-file", "commit", oid])?;
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("committer ") {
            return Ok(rest.to_owned());
        }
    }
    Err("commit object missing committer line".into())
}

fn parse_json(out: &CmdOutput) -> Result<Value, Box<dyn Error>> {
    serde_json::from_str(out.stdout.trim()).map_err(|e| {
        format!(
            "invalid json: {e}\nstdout: {}\nstderr: {}",
            out.stdout, out.stderr
        )
        .into()
    })
}

#[test]
fn history_drop_empty_keep_json() -> TestResult {
    let scratch = Scratch::new("drop-keep")?;
    let dir = scratch.path();
    let (_a, b, _c) = init_undo_redo_repo(dir)?;
    let out = gs(dir, ["history", "drop", &b, "--empty", "keep", "--json"])?;
    assert!(out.success(), "stderr: {}", out.stderr);
    let json = parse_json(&out)?;
    assert_eq!(json["operation"], "drop");
    let rewritten = json["rewritten"].as_array().expect("rewritten array");
    assert!(
        rewritten.iter().any(|row| row["new"].is_string()),
        "expected at least one kept replay commit: {json}"
    );
    git(dir, &["fsck", "--strict"])?;
    Ok(())
}

#[test]
fn history_move_keep_committer_json() -> TestResult {
    let scratch = Scratch::new("move-keep-committer")?;
    let dir = scratch.path();
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("one.txt"), "1\n")?;
    git(dir, &["add", "one.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    fs::write(dir.join("two.txt"), "2\n")?;
    git(dir, &["add", "two.txt"])?;
    git(dir, &["commit", "-m", "second"])?;
    fs::write(dir.join("three.txt"), "3\n")?;
    git(dir, &["add", "three.txt"])?;
    git(dir, &["commit", "-m", "third"])?;
    let moved = git(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    let anchor = git(dir, &["rev-parse", "HEAD~2"])?.trim().to_owned();
    let original_committer = committer_line_from_object(dir, &moved)?;
    let out = gs(
        dir,
        [
            "history",
            "move",
            &moved,
            "--after",
            &anchor,
            "--keep-committer",
            "--json",
        ],
    )?;
    assert!(out.success(), "stderr: {}", out.stderr);
    let json = parse_json(&out)?;
    assert_eq!(json["operation"], "move");
    let new_oid = json["rewritten"]
        .as_array()
        .expect("rewritten")
        .iter()
        .find(|row| row["old"].as_str() == Some(moved.as_str()))
        .and_then(|row| row["new"].as_str())
        .expect("replacement for moved commit");
    assert_eq!(
        committer_line_from_object(dir, new_oid)?,
        original_committer
    );
    git(dir, &["fsck", "--strict"])?;
    Ok(())
}
