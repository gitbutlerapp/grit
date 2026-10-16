//! Integration tests for `grit revert`.

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
        path.push(format!("grit-cli-revert-{tag}-{}-{n}", std::process::id()));
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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
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
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
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

fn init_repo(root: &Path) -> Result<(), Box<dyn Error>> {
    git(root, &["init", "-q", "-b", "main", "."])?;
    git(root, &["config", "user.email", "test@example.com"])?;
    git(root, &["config", "user.name", "Test User"])?;
    git(root, &["config", "gc.auto", "0"])?;
    Ok(())
}

fn commit_file(
    root: &Path,
    path: &str,
    contents: &str,
    msg: &str,
) -> Result<String, Box<dyn Error>> {
    let full = root.join(path);
    if let Some(parent) = full.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&full, contents)?;
    git(root, &["add", path])?;
    git(root, &["commit", "-qm", msg])?;
    Ok(git(root, &["rev-parse", "HEAD"])?.trim().to_owned())
}

fn git_fsck_strict(root: &Path) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git fsck --strict failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

fn snapshot_tree(root: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let mut out = Vec::new();
    for rel in ["index", "HEAD", "refs/heads/main"] {
        let p = root.join(".git").join(rel);
        if p.exists() {
            out.extend_from_slice(rel.as_bytes());
            out.push(0);
            out.extend(fs::read(&p).unwrap_or_default());
        }
    }
    fn walk(dir: &Path, prefix: &Path, out: &mut Vec<u8>) -> Result<(), Box<dyn Error>> {
        if !dir.is_dir() {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.file_name().and_then(|n| n.to_str()) == Some(".git") {
                continue;
            }
            let rel = prefix.join(path.file_name().unwrap());
            if path.is_dir() {
                walk(&path, &rel, out)?;
            } else {
                out.extend(rel.to_string_lossy().as_bytes());
                out.push(0);
                out.extend(fs::read(&path)?);
            }
        }
        Ok(())
    }
    walk(root, Path::new(""), &mut out)?;
    Ok(out)
}

#[test]
fn revert_creates_inverse_commit() -> TestResult {
    let scratch = Scratch::new("inverse")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;

    let parent_tree = git(repo, &["rev-parse", "HEAD~1^{tree}"])?
        .trim()
        .to_owned();

    let out = gs(repo, ["revert", &target])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stdout.contains("Reverted") && out.stdout.contains("Revert"),
        "{}",
        out.dump()
    );

    let head_tree = git(repo, &["rev-parse", "HEAD^{tree}"])?.trim().to_owned();
    assert_eq!(head_tree, parent_tree);
    assert_eq!(fs::read_to_string(repo.join("a.txt"))?, "v1\n");

    let body = git(repo, &["log", "-1", "--format=%B"])?;
    assert!(body.contains("Revert \"feature title\""));
    assert!(body.contains(&format!("This reverts commit {target}.")));

    git_fsck_strict(repo)?;
    Ok(())
}

#[test]
fn revert_conflict_changes_nothing() -> TestResult {
    let scratch = Scratch::new("conflict")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "base\n", "initial")?;
    let target = commit_file(repo, "a.txt", "feature\n", "feature")?;
    git(repo, &["reset", "--hard", "HEAD~1"])?;
    commit_file(repo, "a.txt", "manual\n", "manual")?;

    let before = snapshot_tree(repo)?;
    let out = gs(repo, ["revert", &target])?;
    assert_ne!(out.status, Some(0), "{}", out.dump());
    assert!(
        out.stderr.contains("conflicts") || out.stdout.contains("conflicts"),
        "{}",
        out.dump()
    );
    assert!(
        out.stderr.contains("Nothing was changed") || out.stdout.contains("Nothing was changed"),
        "{}",
        out.dump()
    );
    assert_eq!(before, snapshot_tree(repo)?);
    Ok(())
}

#[test]
fn revert_json_fields() -> TestResult {
    let scratch = Scratch::new("json")?;
    let repo = scratch.path();
    init_repo(repo)?;
    commit_file(repo, "a.txt", "v1\n", "initial")?;
    let target = commit_file(repo, "a.txt", "v2\n", "feature title")?;

    let out = gs(repo, ["--json", "revert", &target])?;
    assert_eq!(out.status, Some(0), "{}", out.dump());
    let v: Value = serde_json::from_str(out.stdout.trim())?;
    assert_eq!(v["source"].as_str(), Some(target.as_str()));
    let oid = v["oid"].as_str().expect("oid");
    assert_eq!(oid.len(), 40);
    assert_eq!(v["subject"].as_str(), Some("Revert \"feature title\""));
    Ok(())
}
