//! Integration tests for `grit history split`.

use std::error::Error;
use std::ffi::{OsStr, OsString};
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
            "grit-cli-history-split-{tag}-{}-{n}",
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

fn null_device() -> OsString {
    if cfg!(windows) {
        OsString::from("NUL")
    } else {
        OsString::from("/dev/null")
    }
}

fn gs<I, S>(dir: &Path, args: I) -> Result<CmdOutput, Box<dyn Error>>
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
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    Ok(CmdOutput {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git(dir: &Path, args: &[&str]) -> Result<String, Box<dyn Error>> {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000001 +0000")
        .env("GIT_COMMITTER_DATE", "1700000002 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_fsck_strict(root: &Path) -> Result<(), Box<dyn Error>> {
    let out = Command::new("git")
        .args(["fsck", "--strict"])
        .current_dir(root)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    if !out.status.success() {
        return Err(format!(
            "git fsck --strict failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )
        .into());
    }
    Ok(())
}

fn parse_json(stdout: &str) -> Result<Value, Box<dyn Error>> {
    Ok(serde_json::from_str(stdout.trim())?)
}

fn change_id_from_cat(cat: &str) -> Option<String> {
    cat.lines()
        .find_map(|line| line.strip_prefix("change-id "))
        .map(str::trim)
        .map(str::to_owned)
}

fn commit_tree(dir: &Path, oid: &str) -> Result<String, Box<dyn Error>> {
    Ok(git(dir, &["rev-parse", &format!("{oid}^{{tree}}")])?
        .trim()
        .to_string())
}

fn tree_has_path(dir: &Path, tree: &str, path: &str) -> Result<bool, Box<dyn Error>> {
    let out = Command::new("git")
        .args(["ls-tree", tree, path])
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    Ok(out.status.success() && !out.stdout.is_empty())
}

fn init_split_repo(dir: &Path) -> Result<(String, String), Box<dyn Error>> {
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("initial.t"), b"x")?;
    git(dir, &["add", "initial.t"])?;
    git(dir, &["commit", "-m", "initial"])?;
    let parent = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    fs::write(dir.join("bar"), b"")?;
    fs::write(dir.join("foo"), b"")?;
    git(dir, &["add", "bar", "foo"])?;
    git(dir, &["commit", "-m", "split-me"])?;
    let target = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    Ok((parent, target))
}

#[test]
fn split_moves_paths_into_new_parent() -> TestResult {
    let scratch = Scratch::new("trees")?;
    let dir = scratch.path();
    let (parent, target) = init_split_repo(dir)?;
    let target_tree_before = commit_tree(dir, &target)?;

    let out = gs(
        dir,
        ["history", "split", &target, "-m", "first half", "--", "foo"],
    )?;
    assert!(out.success(), "grit failed: {}", out.stderr);

    let new_target = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_ne!(new_target, target);
    let new_target_tree = commit_tree(dir, &new_target)?;
    assert_eq!(new_target_tree, target_tree_before, "target tree unchanged");

    let inserted = git(dir, &["rev-parse", &format!("{new_target}^")])?
        .trim()
        .to_string();
    let parent_tree = commit_tree(dir, &parent)?;
    let inserted_tree = commit_tree(dir, &inserted)?;

    assert!(tree_has_path(dir, &inserted_tree, "foo")?);
    assert!(!tree_has_path(dir, &inserted_tree, "bar")?);
    assert!(tree_has_path(dir, &parent_tree, "initial.t")?);
    assert!(!tree_has_path(dir, &parent_tree, "foo")?);

    let diff_names = git(
        dir,
        &["diff", "--name-only", &format!("{parent}..{inserted}")],
    )?;
    assert_eq!(diff_names.trim(), "foo");

    git_fsck_strict(dir)?;
    Ok(())
}

#[test]
fn split_json_shape() -> TestResult {
    let scratch = Scratch::new("json")?;
    let dir = scratch.path();
    let (_parent, target) = init_split_repo(dir)?;

    let out = gs(
        dir,
        [
            "history",
            "split",
            &target,
            "-m",
            "split side",
            "--json",
            "--",
            "foo",
        ],
    )?;
    assert!(out.success(), "grit failed: {}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["operation"].as_str(), Some("split"));
    assert_eq!(json["dry_run"].as_bool(), Some(false));
    assert!(json["rewritten"].is_array());
    assert!(json["refs"].is_array());
    assert!(json.get("head").is_some());

    let rewritten = json["rewritten"].as_array().expect("rewritten");
    let row = rewritten
        .iter()
        .find(|r| r["old"].as_str() == Some(target.as_str()))
        .ok_or("missing target in rewritten")?;
    assert!(row["new"].as_str().is_some());
    assert!(row.get("change_id").is_some());
    Ok(())
}

#[test]
fn split_dry_run() -> TestResult {
    let scratch = Scratch::new("dry")?;
    let dir = scratch.path();
    let (_parent, target) = init_split_repo(dir)?;
    let head_before = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();

    let out = gs(
        dir,
        [
            "history",
            "split",
            &target,
            "-m",
            "split side",
            "--dry-run",
            "--json",
            "--",
            "foo",
        ],
    )?;
    assert!(out.success(), "grit failed: {}", out.stderr);
    let json = parse_json(&out.stdout)?;
    assert_eq!(json["dry_run"].as_bool(), Some(true));
    let head_after = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();
    assert_eq!(head_after, head_before);
    Ok(())
}

#[test]
fn split_selects_nothing_errors() -> TestResult {
    let scratch = Scratch::new("nothing")?;
    let dir = scratch.path();
    let (_parent, target) = init_split_repo(dir)?;

    let out = gs(
        dir,
        [
            "history",
            "split",
            &target,
            "-m",
            "nope",
            "--",
            "missing.txt",
        ],
    )?;
    assert!(!out.success());
    assert!(
        out.stderr.contains("did not select any changes")
            || out.stdout.contains("did not select any changes"),
        "unexpected output: stdout={} stderr={}",
        out.stdout,
        out.stderr
    );
    Ok(())
}

#[test]
fn split_keeps_change_id_on_rest_and_new_id_on_insert() -> TestResult {
    let scratch = Scratch::new("change-id")?;
    let dir = scratch.path();
    git(dir, &["init", "-b", "main"])?;
    fs::write(dir.join("split-a"), b"a")?;
    fs::write(dir.join("split-b"), b"b")?;
    git(dir, &["add", "."])?;
    let commit_out = gs(dir, ["commit", "-m", "split me"])?;
    assert!(commit_out.success(), "commit failed: {}", commit_out.stderr);
    let target = git(dir, &["rev-parse", "HEAD"])?.trim().to_string();

    let log = gs(dir, ["log", "--limit", "1", "--json"])?;
    assert!(log.success());
    let before_json = parse_json(&log.stdout)?;
    let stored_id = before_json["commits"][0]["change_id"]
        .as_str()
        .ok_or("missing change_id on source commit")?
        .to_owned();

    let out = gs(
        dir,
        [
            "history",
            "split",
            &target,
            "-m",
            "inserted\n",
            "--json",
            "--",
            "split-a",
        ],
    )?;
    assert!(out.success(), "grit failed: {}", out.stderr);
    let json = parse_json(&out.stdout)?;
    let row = json["rewritten"]
        .as_array()
        .and_then(|rows| {
            rows.iter()
                .find(|r| r["old"].as_str() == Some(target.as_str()))
        })
        .ok_or("missing rewritten row")?;
    assert_eq!(row["change_id"].as_str(), Some(stored_id.as_str()));

    let new_target = row["new"].as_str().ok_or("missing new oid")?;
    let inserted = git(dir, &["rev-parse", &format!("{new_target}^")])?
        .trim()
        .to_string();
    let inserted_cat = git(dir, &["cat-file", "-p", &inserted])?;
    let inserted_id = change_id_from_cat(&inserted_cat).ok_or("insert missing change-id")?;
    assert_ne!(inserted_id, stored_id);
    git_fsck_strict(dir)?;
    Ok(())
}
