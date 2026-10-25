//! Commit messages and other free-text flags may start with `-` (issue #1519).

use std::error::Error;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

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
            "grit-cli-commit-hyphen-{tag}-{}-{n}",
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

fn git(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn git");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
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
        .env("GIT_AUTHOR_EMAIL", "t@e.test")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.test")
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

#[test]
fn commit_message_flag_leading_hyphen() -> TestResult {
    let scratch = Scratch::new("flag")?;
    let root = scratch.path();
    assert!(
        git(root, &["init", "-q", "-b", "main"]).success(),
        "git init"
    );
    fs::write(root.join("a.txt"), "a\n")?;

    let out = gs(root, ["commit", "-m", "-fix typo", "--json"]);
    assert!(out.success(), "{}", out.dump());

    let json: serde_json::Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["subject"], "-fix typo");
    Ok(())
}

#[test]
fn commit_message_positional_leading_hyphen() -> TestResult {
    let scratch = Scratch::new("positional")?;
    let root = scratch.path();
    assert!(
        git(root, &["init", "-q", "-b", "main"]).success(),
        "git init"
    );
    fs::write(root.join("a.txt"), "a\n")?;

    let out = gs(root, ["commit", "-leading dash message", "--json"]);
    assert!(out.success(), "{}", out.dump());

    let json: serde_json::Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["subject"], "-leading dash message");
    Ok(())
}

fn head(root: &Path) -> Result<String, Box<dyn Error>> {
    let out = git(root, &["rev-parse", "HEAD"]);
    if !out.success() {
        return Err(out.dump().into());
    }
    Ok(out.stdout.trim().to_owned())
}

/// Repository with one commit and a dirty worktree that auto-stage would commit.
fn dirty_repo(tag: &str) -> Result<(Scratch, String), Box<dyn Error>> {
    let scratch = Scratch::new(tag)?;
    let root = scratch.path();
    assert!(
        git(root, &["init", "-q", "-b", "main"]).success(),
        "git init"
    );
    fs::write(root.join("a.txt"), "a\n")?;
    let out = gs(root, ["commit", "-m", "first"]);
    assert!(out.success(), "{}", out.dump());
    fs::write(root.join("a.txt"), "a\nb\n")?;
    let tip = head(root)?;
    Ok((scratch, tip))
}

fn assert_unexpected_argument(out: &CmdOutput, arg: &str) {
    assert_eq!(out.status, Some(2), "{}", out.dump());
    assert!(
        out.stderr
            .contains(&format!("unexpected argument '{arg}' found")),
        "{}",
        out.dump()
    );
}

#[test]
fn commit_unknown_flag_is_rejected_not_used_as_message() -> TestResult {
    for flag in ["--bogus", "-x", "--verbose", "--opt=value"] {
        let (scratch, tip) = dirty_repo("unknown-flag")?;
        let root = scratch.path();
        let out = gs(root, ["commit", flag]);
        assert_unexpected_argument(&out, flag);
        assert_eq!(head(root)?, tip, "{flag} must not create a commit");
    }
    Ok(())
}

#[test]
fn commit_unknown_flag_with_message_reports_unexpected_argument() -> TestResult {
    for args in [
        ["commit", "-m", "x", "--bogus"],
        ["commit", "--bogus", "-m", "x"],
    ] {
        let (scratch, tip) = dirty_repo("unknown-flag-with-m")?;
        let root = scratch.path();
        let out = gs(root, args);
        assert_unexpected_argument(&out, "--bogus");
        assert_eq!(head(root)?, tip);
    }
    Ok(())
}

#[test]
fn commit_quiet_suppresses_human_output_only() -> TestResult {
    let (scratch, tip) = dirty_repo("quiet")?;
    let root = scratch.path();

    let out = gs(root, ["commit", "-q"]);
    assert_eq!(out.status, Some(1), "{}", out.dump());
    assert_eq!(head(root)?, tip, "-q is not a message");

    let out = gs(root, ["commit", "-q", "-m", "real message"]);
    assert!(out.success(), "{}", out.dump());
    assert!(out.stdout.is_empty(), "{}", out.dump());
    let subject = git(root, &["log", "-1", "--format=%s"]);
    assert_eq!(subject.stdout.trim(), "real message");

    fs::write(root.join("a.txt"), "a\nb\nc\n")?;
    let out = gs(root, ["commit", "--quiet", "-m", "json", "--json"]);
    assert!(out.success(), "{}", out.dump());
    let json: serde_json::Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["subject"], "json");
    Ok(())
}

#[test]
fn commit_author_leading_hyphen() -> TestResult {
    let scratch = Scratch::new("author")?;
    let root = scratch.path();
    assert!(
        git(root, &["init", "-q", "-b", "main"]).success(),
        "git init"
    );
    fs::write(root.join("a.txt"), "a\n")?;

    let out = gs(
        root,
        [
            "commit",
            "-m",
            "msg",
            "--author",
            "-Name <n@e.test>",
            "--json",
        ],
    );
    assert!(out.success(), "{}", out.dump());

    let json: serde_json::Value = serde_json::from_str(&out.stdout)?;
    assert_eq!(json["author"]["name"], "-Name");
    assert_eq!(json["author"]["email"], "n@e.test");
    Ok(())
}
