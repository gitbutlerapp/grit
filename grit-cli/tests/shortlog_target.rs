//! `grit shortlog --target` and the `grit ahead` alias.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GS: &str = env!("CARGO_BIN_EXE_grit");

struct Scratch {
    path: PathBuf,
}

impl Scratch {
    fn new(name: &str) -> Result<Self, Box<dyn Error>> {
        let path = std::env::temp_dir().join(format!("grit-shortlog-target-{name}-{}", std::process::id()));
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn child(&self, name: &str) -> PathBuf {
        self.path.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn gs_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GS)
        .args(args)
        .arg("--json")
        .current_dir(dir)
        .output()
        .expect("run grit");
    assert!(
        out.status.success(),
        "grit {} failed: {}",
        args.join(" "),
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn gs_ok(dir: &Path, args: &[&str]) {
    let status = Command::new(GS)
        .args(args)
        .current_dir(dir)
        .status()
        .expect("run grit");
    assert!(status.success(), "grit {} failed", args.join(" "));
}

fn git(repo: &Path, args: &[&str]) {
    assert!(
        Command::new("git")
            .args(args)
            .current_dir(repo)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_AUTHOR_NAME", "T")
            .env("GIT_AUTHOR_EMAIL", "t@e.com")
            .env("GIT_COMMITTER_NAME", "T")
            .env("GIT_COMMITTER_EMAIL", "t@e.com")
            .status()
            .expect("git")
            .success()
    );
}

#[test]
fn shortlog_target_cli() -> TestResult {
    let scratch = Scratch::new("shortlog-target")?;
    let repo = scratch.child("repo");
    fs::create_dir_all(&repo)?;
    git(&repo, &["init", "-q", "-b", "main"]);
    fs::write(repo.join("a"), "1\n")?;
    git(&repo, &["add", "a"]);
    git(&repo, &["commit", "-qm", "m1"]);
    git(&repo, &["checkout", "-qb", "feature"]);
    fs::write(repo.join("b"), "2\n")?;
    git(&repo, &["add", "b"]);
    git(&repo, &["commit", "-qm", "m2"]);

    let v = gs_json(&repo, &["shortlog", "--target", "main"]);
    assert_eq!(v["branch"], "feature");
    assert_eq!(v["target"], "main");
    assert_eq!(v["ahead"], 1);

    let ahead = gs_json(&repo, &["ahead", "--target", "main"]);
    assert_eq!(ahead["ahead"], 1);
    assert_eq!(ahead["target"], "main");

    gs_ok(&repo, &["ahead"]);
    Ok(())
}
