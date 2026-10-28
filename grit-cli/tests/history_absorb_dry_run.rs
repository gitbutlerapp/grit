//! `grit history absorb --dry-run` assignment output and side-effect checks.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::error::Error;
use std::ffi::OsStr;
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
    fn success(&self) -> bool {
        self.status == Some(0)
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
            "grit-cli-history-absorb-dry-{tag}-{}-{n}",
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

fn git_identity() -> [(&'static str, &'static str); 6] {
    [
        ("GIT_AUTHOR_NAME", "Test User"),
        ("GIT_AUTHOR_EMAIL", "test@example.com"),
        ("GIT_COMMITTER_NAME", "Test User"),
        ("GIT_COMMITTER_EMAIL", "test@example.com"),
        ("GIT_AUTHOR_DATE", "1700000001 +0000"),
        ("GIT_COMMITTER_DATE", "1700000002 +0000"),
    ]
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let mut cmd = Command::new("git");
    cmd.args(args).current_dir(dir);
    for (key, value) in git_identity() {
        cmd.env(key, value);
    }
    let out = cmd
        .env("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null"))
        .env("GIT_CONFIG_SYSTEM", OsStr::new("/dev/null"))
        .output()?;
    assert!(out.status.success(), "{:?}", out.stderr);
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn gs(dir: &Path, args: &[&str]) -> Result<CmdOutput, Box<dyn Error>> {
    let mut cmd = Command::new(GS);
    cmd.args(args).current_dir(dir);
    for (key, value) in git_identity() {
        cmd.env(key, value);
    }
    let out = cmd
        .env("GIT_CONFIG_GLOBAL", OsStr::new("/dev/null"))
        .env("GIT_CONFIG_SYSTEM", OsStr::new("/dev/null"))
        .output()?;
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn parse_json(stdout: &str) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(stdout.trim())?)
}

fn init_absorb_fixture(dir: &Path) -> Result<(String, String), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("file.txt"), "line1\nline2\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    let base = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("file.txt"), "line1\nchanged\n")?;
    git(dir, &["add", "file.txt"])?;
    git(dir, &["commit", "-m", "second"])?;
    let target_short = git(dir, &["rev-parse", "--short", "HEAD"])?
        .trim()
        .to_string();
    commit_file(dir, "tail.txt", "t\n", "tail")?;
    fs::write(dir.join("file.txt"), "line1\nchanged\nfix\n")?;
    git(dir, &["add", "file.txt"])?;
    Ok((base, target_short))
}

fn init_absorb_fixture_pipes(dir: &Path) -> Result<String, Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("file|name.txt"), "line1\nline2\n")?;
    git(dir, &["add", "file|name.txt"])?;
    git(dir, &["commit", "-m", "first"])?;
    let base = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("file|name.txt"), "line1\nchanged\n")?;
    git(dir, &["add", "file|name.txt"])?;
    git(dir, &["commit", "-m", "second | pipe"])?;
    commit_file(dir, "tail.txt", "t\n", "tail")?;
    fs::write(dir.join("file|name.txt"), "line1\nchanged\nfix\n")?;
    git(dir, &["add", "file|name.txt"])?;
    Ok(base)
}

fn commit_file(dir: &Path, path: &str, body: &str, message: &str) -> Result<(), Box<dyn Error>> {
    fs::write(dir.join(path), body)?;
    git(dir, &["add", path])?;
    git(dir, &["commit", "-m", message])?;
    Ok(())
}

fn repo_snapshot_bytes(dir: &Path) -> Result<Vec<u8>, Box<dyn Error>> {
    let index = fs::read(dir.join(".git/index"))?;
    let mut refs = git(
        dir,
        &["for-each-ref", "--format=%(refname) %(objectname)", "refs/"],
    )?;
    refs.push_str(&git(dir, &["reflog", "show", "--all", "--format=%gd %gs"])?);
    let mut out = index;
    out.extend_from_slice(refs.as_bytes());
    Ok(out)
}

#[test]
fn absorb_dry_run_lists_assignments_json() -> TestResult {
    let scratch = Scratch::new("json")?;
    let dir = scratch.path();
    let (base, _target_short) = init_absorb_fixture(dir)?;
    let out = gs(
        dir,
        &["history", "absorb", "--base", &base, "--dry-run", "--json"],
    )?;
    assert!(out.success(), "{}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["dry_run"], true);
    assert_eq!(json["operation"], "absorb");
    let assignments = json["assignments"].as_array().expect("assignments array");
    assert_eq!(assignments.len(), 1);
    let row = &assignments[0];
    assert_eq!(row["path"], "file.txt");
    assert_eq!(row["subject"], "second");
    assert_eq!(row["hunk_range"]["old_start"], 2);
    assert_eq!(row["hunk_range"]["old_lines"], 0);
    assert_eq!(row["hunk_range"]["new_start"], 3);
    assert_eq!(row["hunk_range"]["new_lines"], 1);
    let commit = row["commit"].as_str().expect("commit hex");
    assert_eq!(commit.len(), 40);
    let mut snapshot_row = row.clone();
    snapshot_row["commit"] = Value::String("[COMMIT]".into());
    insta::assert_json_snapshot!(snapshot_row, @r#"
    {
      "commit": "[COMMIT]",
      "hunk_range": {
        "new_lines": 1,
        "new_start": 3,
        "old_lines": 0,
        "old_start": 2
      },
      "path": "file.txt",
      "subject": "second"
    }
    "#);
    Ok(())
}

#[test]
fn absorb_dry_run_leaves_index_refs_worktree_unchanged() -> TestResult {
    let scratch = Scratch::new("unchanged")?;
    let dir = scratch.path();
    let (base, _) = init_absorb_fixture(dir)?;
    let before = repo_snapshot_bytes(dir)?;
    let worktree = fs::read(dir.join("file.txt"))?;
    let out = gs(
        dir,
        &["history", "absorb", "--base", &base, "--dry-run", "--json"],
    )?;
    assert!(out.success(), "{}", out.stderr);
    let after = repo_snapshot_bytes(dir)?;
    assert_eq!(before, after, "index, refs, and reflogs must be unchanged");
    assert_eq!(worktree, fs::read(dir.join("file.txt"))?);
    Ok(())
}

#[test]
fn absorb_dry_run_with_backup_branch_at_head() -> TestResult {
    let scratch = Scratch::new("backup-head")?;
    let dir = scratch.path();
    git(dir, &["init", "-b", "main"])?;
    for i in 1..=3 {
        let body: String = (1..=5).map(|n| format!("f{i}-{n}\n")).collect();
        fs::write(dir.join(format!("f{i}.txt")), body)?;
    }
    git(dir, &["add", "."])?;
    git(dir, &["commit", "-m", "base"])?;
    git(dir, &["checkout", "-b", "feature"])?;
    for i in 1..=3 {
        let path = dir.join(format!("f{i}.txt"));
        let mut text = fs::read_to_string(&path)?;
        text.push_str(&format!("line {i}\n"));
        fs::write(&path, text)?;
        git(dir, &["add", "."])?;
        git(dir, &["commit", "-m", &format!("feature: touch f{i}")])?;
    }
    git(dir, &["branch", "backup"])?;
    let mut text = fs::read_to_string(dir.join("f1.txt"))?;
    text = text.replace("line 1\n", "line 1 fixed\n");
    fs::write(dir.join("f1.txt"), text)?;
    git(dir, &["add", "f1.txt"])?;
    let out = gs(dir, &["history", "absorb", "--dry-run", "--json"])?;
    assert!(out.success(), "{}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["assignments"].as_array().map(|a| a.len()), Some(1));
    assert!(
        !json["excluded_refs"]
            .as_array()
            .is_some_and(|refs| refs.iter().any(|r| r["ref"] == "refs/heads/backup")),
        "{}",
        out.stdout
    );
    let human = gs(dir, &["history", "absorb", "--dry-run"])?;
    assert!(human.success(), "{}", human.stderr);
    assert!(
        human.stdout.contains("feature: touch f1"),
        "{}",
        human.stdout
    );
    Ok(())
}

#[test]
fn absorb_dry_run_markdown_snapshot() -> TestResult {
    let scratch = Scratch::new("md")?;
    let dir = scratch.path();
    let base = init_absorb_fixture_pipes(dir)?;
    let out = gs(
        dir,
        &[
            "history",
            "absorb",
            "--base",
            &base,
            "--dry-run",
            "--markdown",
        ],
    )?;
    assert!(out.success(), "{}", out.stderr);
    insta::with_settings!({filters => vec![
        (r"[0-9a-f]{7}\b", "[COMMIT]"),
        (r"change id `[a-z]{7}`", "change id `[ID]`"),
    ]}, {
        insta::assert_snapshot!(out.stdout, @r#"
        ## History absorb

        - Dry run (refs unchanged)

        ### Assigned hunks

        | Path | Hunk | Commit | Subject |
        | --- | --- | --- | --- |
        | `file\|name.txt` | `@@ -2,0 +3 @@` | `[COMMIT]` | second \| pipe |

        - commit `[COMMIT]` → `[COMMIT]` — second | pipe
          - change id `[ID]`
        - commit `[COMMIT]` → `[COMMIT]` — tail
          - change id `[ID]`
        - `refs/heads/main` `[COMMIT]` → `[COMMIT]`
        - HEAD `[COMMIT]` → `[COMMIT]`
        "#);
    });
    Ok(())
}
