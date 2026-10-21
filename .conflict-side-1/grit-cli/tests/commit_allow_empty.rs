//! GitHub issue #1013: grit commit --allow-empty on a clean tree.

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
            "grit-cli-commit-allow-empty-{tag}-{}-{n}",
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

#[test]
fn allow_empty_commit_on_clean_tree() -> TestResult {
    let scratch = Scratch::new("1013")?;
    let root = scratch.path();
    gs(root, ["init"]);
    fs::write(root.join("a.txt"), b"a\n")?;
    let first = gs(root, ["commit", "-m", "one"]);
    assert_eq!(first.status, Some(0), "{}", first.dump());

    let refused = gs(root, ["commit", "-m", "empty"]);
    assert_ne!(refused.status, Some(0), "{}", refused.dump());
    assert!(
        refused.stderr.contains("nothing to commit"),
        "expected clean-tree refusal: {}",
        refused.dump()
    );

    let empty = gs(root, ["commit", "--allow-empty", "-m", "empty"]);
    assert_eq!(empty.status, Some(0), "{}", empty.dump());
    assert!(
        empty.stdout.contains("0 change"),
        "expected zero changes reported: {}",
        empty.dump()
    );

    let json = gs(root, ["commit", "--allow-empty", "-m", "json", "--json"]);
    assert_eq!(json.status, Some(0), "{}", json.dump());
    let parsed: serde_json::Value = serde_json::from_str(json.stdout.trim())?;
    assert_eq!(parsed.get("changes").and_then(|v| v.as_u64()), Some(0));

    Ok(())
}

#[test]
fn allow_empty_still_stages_tracked_edits_before_commit() -> TestResult {
    let scratch = Scratch::new("allow-empty-dirty")?;
    let root = scratch.path();
    gs(root, ["init"]);
    fs::write(root.join("a.txt"), b"base\n")?;
    assert_eq!(gs(root, ["commit", "-m", "base"]).status, Some(0));

    fs::write(root.join("a.txt"), b"dirty\n")?;

    let empty = gs(root, ["commit", "--allow-empty", "-m", "marker"]);
    assert_eq!(empty.status, Some(0), "{}", empty.dump());
    assert!(
        empty.stdout.contains("1 change"),
        "expected staged edit in commit output: {}",
        empty.dump()
    );

    let status = gs(root, ["status", "--json"]);
    assert_eq!(status.status, Some(0), "{}", status.dump());
    let parsed: serde_json::Value = serde_json::from_str(status.stdout.trim())?;
    assert_eq!(
        parsed.get("clean").and_then(|v| v.as_bool()),
        Some(true),
        "working tree must be clean after commit staged the edit: {}",
        status.dump()
    );

    Ok(())
}

#[test]
fn allow_empty_still_stages_modified_tracked_file() -> TestResult {
    let scratch = Scratch::new("allow-empty-dirty-wt")?;
    let root = scratch.path();
    gs(root, ["init"]);
    fs::write(root.join("tracked.txt"), b"v1\n")?;
    let first = gs(root, ["commit", "-m", "one"]);
    assert_eq!(first.status, Some(0), "{}", first.dump());

    fs::write(root.join("tracked.txt"), b"v2\n")?;

    let empty = gs(root, ["commit", "--allow-empty", "-m", "stage-dirty"]);
    assert_eq!(empty.status, Some(0), "{}", empty.dump());
    assert!(
        !empty.stdout.contains("0 change"),
        "expected staged worktree edit in commit: {}",
        empty.dump()
    );

    let status = gs(root, ["status"]);
    assert_eq!(status.status, Some(0), "{}", status.dump());
    assert!(
        !status.stdout.contains("modified:") && !status.stdout.contains("untracked:"),
        "worktree must be clean after allow-empty with dirty file: {}",
        status.dump()
    );

    let on_disk = fs::read_to_string(root.join("tracked.txt"))?;
    assert_eq!(on_disk, "v2\n");

    Ok(())
}
