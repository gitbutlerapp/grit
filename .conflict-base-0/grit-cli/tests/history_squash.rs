//! Integration tests for `grit history squash`.

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
            "grit-cli-history-squash-{tag}-{}-{n}",
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

fn parse_json(stdout: &str) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(stdout.trim())?)
}

fn init_linear_repo(dir: &Path) -> Result<(String, String, String), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("file.txt"), b"v1\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    let a = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("file.txt"), b"v2\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "second"])?;
    let b = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("file.txt"), b"v3\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "third"])?;
    let c = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok((a, b, c))
}

#[test]
fn history_squash_json_head_maps_through_ref_update() -> TestResult {
    let scratch = Scratch::new("head-json")?;
    let dir = scratch.path();
    let (a, _b, c) = init_linear_repo(dir)?;
    let head_before = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_eq!(head_before, c);
    let range = format!("{a}..{c}");
    let out = gs(
        dir,
        [
            "history",
            "squash",
            &range,
            "--update-refs",
            "head",
            "--json",
        ],
    )?;
    assert!(out.success(), "grit failed: {}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"], "squash");
    let rewritten = json["rewritten"].as_array().ok_or("rewritten array")?;
    assert!(
        rewritten.iter().all(|row| row.get("subject").is_some()),
        "each rewritten row must include subject: {json}"
    );
    let refs = json["refs"].as_array().ok_or("refs array")?;
    assert!(
        refs.iter()
            .all(|row| row.get("ref").is_some() && row.get("name").is_some()),
        "each ref row must include ref and deprecated name: {json}"
    );
    let head = json["head"]
        .as_object()
        .ok_or("head must be an object when HEAD commit changes")?;
    assert_eq!(head["old"].as_str(), Some(head_before.as_str()));
    let head_after = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_eq!(head["new"].as_str(), Some(head_after.as_str()));
    assert_ne!(head["old"], head["new"]);
    Ok(())
}
