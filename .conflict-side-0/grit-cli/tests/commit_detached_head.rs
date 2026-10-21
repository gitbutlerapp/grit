//! GitHub issue #956: grit commit on detached HEAD must fail without staging.

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

#[test]
fn commit_on_detached_head_fails_without_staging() -> TestResult {
    let scratch = Scratch::new("956")?;
    let root = scratch.path();
    gs(root, ["init"]);
    git_ok(root, &["config", "user.name", "R"]);
    git_ok(root, &["config", "user.email", "r@e.com"]);
    fs::write(root.join("a.txt"), b"a\n")?;
    let first = gs(root, ["commit", "-m", "one"]);
    assert_eq!(first.status, Some(0), "{}", first.dump());
    git_ok(root, &["checkout", "--detach"]);
    fs::write(root.join("e.txt"), b"y\n")?;
    assert_eq!(git_status_short(root).trim(), "?? e.txt");

    let human = gs(root, ["commit", "-m", "x"]);
    assert_ne!(human.status, Some(0), "{}", human.dump());
    assert!(
        human.stderr.contains("detached"),
        "human stderr should mention detached HEAD: {}",
        human.dump()
    );
    assert_eq!(
        git_status_short(root).trim(),
        "?? e.txt",
        "index must stay unchanged after refused commit"
    );

    let amend = gs(root, ["commit", "--amend", "-m", "amend"]);
    assert_ne!(amend.status, Some(0), "{}", amend.dump());
    assert_eq!(
        git_status_short(root).trim(),
        "?? e.txt",
        "amend on detached HEAD must not stage either"
    );

    Ok(())
}
