//! Issue #2522: case-only directory renames must update on-disk directory spelling on
//! case-insensitive volumes when switching, merging, picking, or restoring from a tree.

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
        path.push(format!("grit-cli-{tag}-{}-{n}", std::process::id()));
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

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

struct CmdOutput {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

fn gs(dir: &Path, args: &[&str]) -> Result<CmdOutput, Box<dyn Error>> {
    let args: Vec<OsString> = args.iter().map(OsStr::from).map(OsString::from).collect();
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

fn gs_ok(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = gs(dir, args)?;
    assert!(
        out.status == Some(0),
        "grit {args:?} failed: exit={:?}\nstdout:\n{}\nstderr:\n{}",
        out.status,
        out.stdout,
        out.stderr
    );
    Ok(())
}

fn git_ok(dir: &Path, args: &[&str]) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(out.status.success(), "git {args:?}: {}", out.status);
    Ok(())
}

fn worktree_filesystem_case_insensitive(root: &Path) -> bool {
    let probe = root.join(".grit_icase_probe");
    if fs::write(&probe, b"x").is_err() {
        return false;
    }
    let folds = root.join(".GRIT_ICASE_PROBE").exists();
    let _ = fs::remove_file(&probe);
    folds
}

/// Actual directory name on disk for a path component that folds to `expected`.
fn directory_spelling(work_tree: &Path, expected: &str) -> Option<String> {
    fs::read_dir(work_tree).ok()?.find_map(|entry| {
        let entry = entry.ok()?;
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        if lossy.eq_ignore_ascii_case(expected) {
            Some(lossy.into_owned())
        } else {
            None
        }
    })
}

fn init_issue_2522_fixture(scratch: &Scratch) -> Result<(String, String), Box<dyn Error>> {
    let repo = scratch.path();
    git_ok(repo, &["init", "-q", "-b", "main", "."])?;
    git_ok(repo, &["config", "user.email", "a@b"])?;
    git_ok(repo, &["config", "user.name", "A"])?;
    git_ok(repo, &["config", "core.ignorecase", "true"])?;
    fs::create_dir_all(repo.join("Foo"))?;
    fs::write(repo.join("Foo/a.txt"), "1\n")?;
    git_ok(repo, &["add", "."])?;
    git_ok(repo, &["commit", "-qm", "base"])?;
    let blob_oid = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "HEAD:Foo/a.txt"])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    git_ok(repo, &["switch", "-qc", "lower"])?;
    git_ok(repo, &["rm", "-q", "--cached", "-r", "Foo"])?;
    git_ok(
        repo,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob_oid},foo/a.txt"),
        ],
    )?;
    git_ok(repo, &["commit", "-qm", "lower"])?;
    git_ok(repo, &["switch", "-q", "main"])?;
    let lower_tip = {
        let out = Command::new("git")
            .current_dir(repo)
            .args(["rev-parse", "lower"])
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_CONFIG_SYSTEM", null_device())
            .output()?;
        assert!(out.status.success());
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    };
    Ok((lower_tip, "lower".to_owned()))
}

fn assert_directory_spelled_foo(repo: &Path) -> Result<(), Box<dyn Error>> {
    assert_eq!(
        directory_spelling(repo, "foo").as_deref(),
        Some("foo"),
        "work tree directory must match tree spelling 'foo'"
    );
    Ok(())
}

#[test]
fn switch_merge_pick_restore_use_lower_case_directory_spelling() -> TestResult {
    let probe = Scratch::new("issue-2522-probe")?;
    if !worktree_filesystem_case_insensitive(probe.path()) {
        return Ok(());
    }

    let switch_scratch = Scratch::new("issue-2522-switch")?;
    init_issue_2522_fixture(&switch_scratch)?;
    let repo = switch_scratch.path();
    gs_ok(repo, &["switch", "lower"])?;
    assert_directory_spelled_foo(repo)?;

    let merge_scratch = Scratch::new("issue-2522-merge")?;
    let (_, branch) = init_issue_2522_fixture(&merge_scratch)?;
    let repo = merge_scratch.path();
    gs_ok(repo, &["merge", &branch])?;
    assert_directory_spelled_foo(repo)?;

    let pick_scratch = Scratch::new("issue-2522-pick")?;
    let (lower_tip, _) = init_issue_2522_fixture(&pick_scratch)?;
    let repo = pick_scratch.path();
    gs_ok(repo, &["pick", &lower_tip])?;
    assert_directory_spelled_foo(repo)?;

    let restore_scratch = Scratch::new("issue-2522-restore")?;
    init_issue_2522_fixture(&restore_scratch)?;
    let repo = restore_scratch.path();
    gs_ok(repo, &["restore", "--source", "lower", "--", "foo"])?;
    assert_directory_spelled_foo(repo)?;

    Ok(())
}
