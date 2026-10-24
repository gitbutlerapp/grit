//! Detached HEAD commits (GitHub issue #1513) and failed-commit index safety (#956).

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
            "grit-cli-commit-detached-{tag}-{}-{n}",
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
        .env("GIT_AUTHOR_NAME", "R")
        .env("GIT_AUTHOR_EMAIL", "r@e.com")
        .env("GIT_COMMITTER_NAME", "R")
        .env("GIT_COMMITTER_EMAIL", "r@e.com")
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

fn git(dir: &Path, args: &[&str]) -> CmdOutput {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "R")
        .env("GIT_AUTHOR_EMAIL", "r@e.com")
        .output()
        .expect("git");
    CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

fn git_ok(dir: &Path, args: &[&str]) {
    let out = git(dir, args);
    assert!(out.status == Some(0), "git {:?}: {}", args, out.dump());
}

fn git_status_short(dir: &Path) -> String {
    git_ok(dir, &["status", "--short"]);
    git(dir, &["status", "--short"]).stdout
}

fn head_oid(dir: &Path) -> String {
    git_ok(dir, &["rev-parse", "HEAD"]);
    git(dir, &["rev-parse", "HEAD"]).stdout.trim().to_owned()
}

#[test]
fn commit_on_detached_head_advances_head_and_git_fsck() -> TestResult {
    let scratch = Scratch::new("1513")?;
    let root = scratch.path();
    gs(root, ["init"]);
    git_ok(root, &["config", "user.name", "R"]);
    git_ok(root, &["config", "user.email", "r@e.com"]);
    fs::write(root.join("a.txt"), b"a\n")?;
    let first = gs(root, ["commit", "-m", "one"]);
    assert_eq!(first.status, Some(0), "{}", first.dump());
    let tip_before_detach = head_oid(root);
    git_ok(root, &["checkout", "--detach"]);
    assert_eq!(head_oid(root), tip_before_detach);

    fs::write(root.join("d.txt"), b"d\n")?;
    assert_eq!(git_status_short(root).trim(), "?? d.txt");

    let human = gs(root, ["commit", "-m", "detached"]);
    assert_eq!(human.status, Some(0), "{}", human.dump());
    assert!(
        human.stdout.contains("detached HEAD"),
        "human output should label detached HEAD: {}",
        human.dump()
    );
    assert_ne!(head_oid(root), tip_before_detach);
    assert_eq!(git_status_short(root).trim(), "");

    git_ok(root, &["fsck", "--strict"]);

    Ok(())
}

#[test]
fn detached_head_is_null_branch_in_commit_and_switch_json() -> TestResult {
    let scratch = Scratch::new("1798-json")?;
    let root = scratch.path();
    gs(root, ["init"]);
    fs::write(root.join("a.txt"), b"a\n")?;
    let first = gs(root, ["--json", "commit", "-m", "one"]);
    assert_eq!(first.status, Some(0), "{}", first.dump());
    let first: serde_json::Value = serde_json::from_str(&first.stdout)?;
    assert_eq!(first["branch"], "main");
    assert_eq!(first["detached"], false);
    let tip = head_oid(root);

    let switched = gs(root, ["--json", "switch", "--detach", "HEAD"]);
    assert_eq!(switched.status, Some(0), "{}", switched.dump());
    let switched: serde_json::Value = serde_json::from_str(&switched.stdout)?;
    assert!(switched["branch"].is_null(), "{switched}");
    assert_eq!(switched["detached"], true);
    assert_eq!(switched["commit"], tip.as_str());

    fs::write(root.join("b.txt"), b"b\n")?;
    let committed = gs(root, ["--json", "commit", "-m", "two"]);
    assert_eq!(committed.status, Some(0), "{}", committed.dump());
    let committed: serde_json::Value = serde_json::from_str(&committed.stdout)?;
    assert!(committed["branch"].is_null(), "{committed}");
    assert_eq!(committed["detached"], true);

    let markdown = gs(root, ["--markdown", "switch", "main"]);
    assert_eq!(markdown.status, Some(0), "{}", markdown.dump());
    assert!(
        markdown.stdout.contains("Switched to **`main`**"),
        "{}",
        markdown.dump()
    );
    Ok(())
}

#[test]
fn failed_commit_on_detached_head_does_not_mutate_head() -> TestResult {
    let scratch = Scratch::new("956-fail")?;
    let root = scratch.path();
    gs(root, ["init"]);
    git_ok(root, &["config", "user.name", "R"]);
    git_ok(root, &["config", "user.email", "r@e.com"]);
    fs::write(root.join("a.txt"), b"a\n")?;
    assert_eq!(gs(root, ["commit", "-m", "one"]).status, Some(0));
    git_ok(root, &["checkout", "--detach"]);
    let head_at_detach = head_oid(root);

    let nothing = gs(root, ["commit", "-m", "noop"]);
    assert_ne!(nothing.status, Some(0), "{}", nothing.dump());
    assert_eq!(
        head_oid(root),
        head_at_detach,
        "nothing-to-commit must not advance detached HEAD"
    );

    Ok(())
}
