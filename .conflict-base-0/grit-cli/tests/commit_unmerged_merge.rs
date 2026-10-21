//! GitHub issue #938: grit commit during an unresolved merge must fail.

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
            "grit-cli-commit-unmerged-{tag}-{}-{n}",
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

fn git(dir: &Path, args: &[&str]) {
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
fn commit_during_unresolved_merge_auto_stages_like_git_commit_a() -> TestResult {
    let scratch = Scratch::new("938")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f"), b"base\n")?;
    git(root, &["add", "f"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    fs::write(root.join("f"), b"side\n")?;
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join("f"), b"main\n")?;
    git(root, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .current_dir(root)
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(!merge.status.success(), "merge should conflict");

    let git_commit = Command::new("git")
        .current_dir(root)
        .args(["commit", "-m", "oops"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(
        !git_commit.status.success(),
        "plain git commit should refuse unmerged index"
    );

    let human = gs(root, ["commit", "-m", "oops"]);
    assert_eq!(
        human.status,
        Some(0),
        "grit commit auto-stages conflicted paths like git commit -a: {}",
        human.dump()
    );
    assert!(
        !root.join(".git/MERGE_HEAD").exists(),
        "merge commit should clear MERGE_HEAD"
    );
    let show = Command::new("git")
        .current_dir(root)
        .args(["show", "HEAD:f"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    let body = String::from_utf8_lossy(&show.stdout);
    assert!(
        body.contains("<<<<<<<"),
        "auto-staged merge records worktree conflict markers: {body}"
    );
    Ok(())
}

#[test]
fn commit_after_resolved_merge_without_add_succeeds() -> TestResult {
    let scratch = Scratch::new("971")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("f.txt"), b"base\n")?;
    git(root, &["add", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    fs::write(root.join("f.txt"), b"side\n")?;
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join("f.txt"), b"main\n")?;
    git(root, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .current_dir(root)
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(!merge.status.success(), "merge should conflict");

    fs::write(root.join("f.txt"), b"resolved\n")?;

    let commit = gs(root, ["commit", "-m", "merged"]);
    assert_eq!(commit.status, Some(0), "{}", commit.dump());

    assert!(
        !root.join(".git/MERGE_HEAD").exists(),
        "MERGE_HEAD should be cleared after merge commit"
    );

    let parents = Command::new("git")
        .current_dir(root)
        .args(["rev-list", "--parents", "-1", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(parents.status.success());
    let line = String::from_utf8_lossy(&parents.stdout);
    let parent_count = line.split_whitespace().count();
    assert_eq!(
        parent_count, 3,
        "merge commit should have two parents: {line}"
    );

    let blob = Command::new("git")
        .current_dir(root)
        .args(["show", "HEAD:f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(blob.status.success());
    assert_eq!(String::from_utf8_lossy(&blob.stdout).trim(), "resolved");
    Ok(())
}

#[test]
fn commit_after_resolved_merge_succeeds_when_unrelated_file_has_marker_literals() -> TestResult {
    let scratch = Scratch::new("971-fp")?;
    let root = scratch.path();
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(
        root.join("guide.txt"),
        b"example markers: <<<<<<< ours and >>>>>>> theirs\n",
    )?;
    fs::write(root.join("f.txt"), b"base\n")?;
    git(root, &["add", "guide.txt", "f.txt"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["checkout", "-qb", "side"]);
    fs::write(root.join("f.txt"), b"side\n")?;
    git(root, &["commit", "-qam", "side"]);
    git(root, &["checkout", "-q", "main"]);
    fs::write(root.join("f.txt"), b"main\n")?;
    git(root, &["commit", "-qam", "main"]);
    let merge = Command::new("git")
        .current_dir(root)
        .args(["merge", "side"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    assert!(!merge.status.success(), "merge should conflict");

    fs::write(root.join("f.txt"), b"resolved\n")?;

    let commit = gs(root, ["commit", "-m", "merged"]);
    assert_eq!(commit.status, Some(0), "{}", commit.dump());

    let git_commit = Command::new("git")
        .current_dir(root)
        .args(["rev-list", "--parents", "-1", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()?;
    let parents_line = String::from_utf8_lossy(&git_commit.stdout);
    assert_eq!(
        parents_line.split_whitespace().count(),
        3,
        "two-parent merge commit"
    );
    Ok(())
}
