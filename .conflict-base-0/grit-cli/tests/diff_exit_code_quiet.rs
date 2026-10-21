//! `grit diff --exit-code`, `--quiet`, `--cached`, and JSON `changed`.

use std::error::Error;
use std::ffi::{OsStr, OsString};
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
        path.push(format!("grit-diff-exit-{tag}-{}-{n}", std::process::id()));
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

fn gs<I, S>(dir: &Path, args: I) -> Result<(Option<i32>, String, String), Box<dyn Error>>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let args: Vec<OsString> = args
        .into_iter()
        .map(|arg| arg.as_ref().to_os_string())
        .collect();
    let out = Command::new(GS).args(&args).current_dir(dir).output()?;
    Ok((
        out.status.code(),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    ))
}

fn git_ok(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git");
    assert!(status.success(), "git {:?} failed", args);
}

fn git_diff_exit(dir: &Path, args: &[&str]) -> i32 {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .status()
        .expect("git diff")
        .code()
        .unwrap_or(1)
}

#[test]
fn diff_exit_code_matches_git() -> TestResult {
    let scratch = Scratch::new("exit-code")?;
    let repo = scratch.path();
    git_ok(repo, &["init", "-q", "-b", "main"]);
    git_ok(repo, &["config", "user.name", "T"]);
    git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f.txt"), b"one\n")?;
    git_ok(repo, &["add", "f.txt"]);
    git_ok(repo, &["commit", "-qm", "init"]);

    for (git_args, grit_args) in [
        (vec!["diff", "--exit-code"], vec!["diff", "--exit-code"]),
        (
            vec!["diff", "--cached", "--exit-code"],
            vec!["diff", "--staged", "--exit-code"],
        ),
    ] {
        let git_code = git_diff_exit(repo, &git_args);
        let (grit_code, _, _) = gs(repo, grit_args)?;
        assert_eq!(grit_code, Some(git_code), "grit vs git for {:?}", git_args);
    }

    fs::write(repo.join("f.txt"), b"two\n")?;
    let git_code = git_diff_exit(repo, &["diff", "--exit-code"]);
    let (grit_code, _, _) = gs(repo, ["diff", "--exit-code"])?;
    assert_eq!(grit_code, Some(git_code));

    Ok(())
}

#[test]
fn diff_quiet_prints_nothing() -> TestResult {
    let scratch = Scratch::new("quiet")?;
    let repo = scratch.path();
    git_ok(repo, &["init", "-q", "-b", "main"]);
    git_ok(repo, &["config", "user.name", "T"]);
    git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f.txt"), b"one\n")?;
    git_ok(repo, &["add", "f.txt"]);
    git_ok(repo, &["commit", "-qm", "init"]);
    fs::write(repo.join("f.txt"), b"two\n")?;

    let (code, stdout, stderr) = gs(repo, ["diff", "--quiet"])?;
    assert_eq!(code, Some(1));
    assert!(stdout.is_empty(), "stdout must be empty: {stdout:?}");
    assert!(stderr.is_empty(), "stderr must be empty: {stderr:?}");

    let (code, stdout, _) = gs(repo, ["--json", "diff", "--quiet"])?;
    assert_eq!(code, Some(1));
    assert!(stdout.is_empty(), "json quiet must not print: {stdout:?}");
    Ok(())
}

#[test]
fn diff_cached_is_staged() -> TestResult {
    let scratch = Scratch::new("cached")?;
    let repo = scratch.path();
    git_ok(repo, &["init", "-q", "-b", "main"]);
    git_ok(repo, &["config", "user.name", "T"]);
    git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f.txt"), b"one\n")?;
    git_ok(repo, &["add", "f.txt"]);
    git_ok(repo, &["commit", "-qm", "init"]);
    fs::write(repo.join("f.txt"), b"staged\n")?;
    git_ok(repo, &["add", "f.txt"]);

    let git_code = git_diff_exit(repo, &["diff", "--cached", "--exit-code"]);
    let (grit_code, _, _) = gs(repo, ["diff", "--cached", "--exit-code"])?;
    assert_eq!(grit_code, Some(git_code));
    Ok(())
}

#[test]
fn diff_json_has_changed_field() -> TestResult {
    let scratch = Scratch::new("json-changed")?;
    let repo = scratch.path();
    git_ok(repo, &["init", "-q", "-b", "main"]);
    git_ok(repo, &["config", "user.name", "T"]);
    git_ok(repo, &["config", "user.email", "t@e.com"]);
    fs::write(repo.join("f.txt"), b"one\n")?;
    git_ok(repo, &["add", "f.txt"]);
    git_ok(repo, &["commit", "-qm", "init"]);

    let (_, stdout, _) = gs(repo, ["--json", "diff"])?;
    let clean: serde_json::Value = serde_json::from_str(&stdout)?;
    assert_eq!(clean.get("changed").and_then(|v| v.as_bool()), Some(false));

    fs::write(repo.join("f.txt"), b"two\n")?;
    let (_, stdout, _) = gs(repo, ["--json", "diff"])?;
    let dirty: serde_json::Value = serde_json::from_str(&stdout)?;
    assert_eq!(dirty.get("changed").and_then(|v| v.as_bool()), Some(true));
    Ok(())
}
