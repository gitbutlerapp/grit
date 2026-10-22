//! Commit must stage untracked files in deep directories unrelated to tracked edits.

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
        path.push(format!(
            "grit-cli-commit-deep-untr-{tag}-{}-{n}",
            std::process::id()
        ));
        if path.exists() {
            fs::remove_dir_all(&path)?;
        }
        fs::create_dir_all(&path)?;
        Ok(Self { path })
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

fn gs<I, S>(dir: &Path, args: I) -> std::process::Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(GS)
        .args(
            args.into_iter()
                .map(|a| a.as_ref().to_os_string())
                .collect::<Vec<OsString>>(),
        )
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@e.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@e.com")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("spawn grit")
}

fn git_ok(dir: &Path, args: &[&str]) {
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
}

#[test]
fn commit_stages_deep_unrelated_untracked_and_passes_fsck() -> TestResult {
    let scratch = Scratch::new("deep")?;
    let root = &scratch.path;
    git_ok(root, &["init", "-q", "-b", "main"]);
    git_ok(root, &["config", "user.name", "T"]);
    git_ok(root, &["config", "user.email", "t@e.com"]);

    fs::create_dir_all(root.join("zone-a"))?;
    fs::write(root.join("zone-a/tracked.txt"), b"base\n")?;
    git_ok(root, &["add", "zone-a/tracked.txt"]);
    git_ok(root, &["commit", "-qm", "base"]);

    fs::write(root.join("zone-a/tracked.txt"), b"edited\n")?;
    let deep = root.join("zone-b/level1/level2/level3/nested-new.txt");
    fs::create_dir_all(deep.parent().unwrap())?;
    fs::write(&deep, b"brand new\n")?;

    let out = gs(root, ["commit", "-m", "capture deep untracked"]);
    assert!(
        out.status.success(),
        "grit commit failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let tree = Command::new("git")
        .current_dir(root)
        .args(["ls-tree", "-r", "--name-only", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(tree.status.success());
    let names = String::from_utf8_lossy(&tree.stdout);
    assert!(
        names.contains("zone-b/level1/level2/level3/nested-new.txt"),
        "deep untracked must be in commit tree:\n{names}"
    );
    assert!(names.contains("zone-a/tracked.txt"));

    git_ok(root, &["fsck", "--no-progress"]);

    let porcelain = Command::new("git")
        .current_dir(root)
        .args(["status", "--porcelain"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(porcelain.status.success());
    assert!(
        String::from_utf8_lossy(&porcelain.stdout).trim().is_empty(),
        "worktree should be clean after commit"
    );
    Ok(())
}
