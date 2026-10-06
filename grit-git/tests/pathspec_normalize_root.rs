//! Regression for repo-root pathspecs with leading `./` (t7501.76).
//!
//! `git add ./-` must stage `-`, not a literal `./-` path that would produce an invalid `.`
//! tree entry on write-tree / checkout.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU32, Ordering};

const GRIT_BIN: &str = env!("CARGO_BIN_EXE_grit-git");

struct Output {
    status: Option<i32>,
    stdout: String,
    stderr: String,
}

impl Output {
    fn ok(&self) -> bool {
        self.status == Some(0)
    }
}

fn unique_tmp(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    let mut p = std::env::temp_dir();
    p.push(format!("grit-pathspec-{tag}-{}-{n}", std::process::id()));
    let _ = std::fs::remove_dir_all(&p);
    std::fs::create_dir_all(&p).expect("create temp dir");
    p
}

fn grit(args: &[&str], dir: &Path) -> Output {
    let out = Command::new(GRIT_BIN)
        .args(args)
        .current_dir(dir)
        .env("GIT_AUTHOR_NAME", "Test")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .env("GIT_AUTHOR_DATE", "1700000000 +0000")
        .env("GIT_COMMITTER_DATE", "1700000000 +0000")
        .output()
        .unwrap_or_else(|e| panic!("failed to spawn grit-git {args:?}: {e}"));
    Output {
        status: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

#[test]
fn add_dot_slash_dash_writes_tree_without_dot_entry() {
    let dir = unique_tmp("add-dot-dash");
    assert!(grit(&["init"], &dir).ok(), "init failed");

    std::fs::write(dir.join("-"), "body\n").expect("create file named '-'");

    let add = grit(&["add", "./-"], &dir);
    assert!(add.ok(), "add ./- failed: {}", add.stderr);

    let wt = grit(&["write-tree"], &dir);
    assert!(wt.ok(), "write-tree failed: {}", wt.stderr);
    let tree_oid = wt.stdout.trim();

    let ls = grit(&["ls-tree", tree_oid], &dir);
    assert!(ls.ok(), "ls-tree failed: {}", ls.stderr);
    assert!(
        ls.stdout.lines().any(|line| line.ends_with("\t-")),
        "expected tree entry named '-', got:\n{}",
        ls.stdout
    );
    assert!(
        !ls.stdout.lines().any(|line| line.ends_with("\t.")),
        "tree must not contain a '.' entry:\n{}",
        ls.stdout
    );

    let commit = grit(&["commit", "-m", "add dash"], &dir);
    assert!(commit.ok(), "commit failed: {}", commit.stderr);
    let head = grit(&["rev-parse", "HEAD"], &dir);
    assert!(head.ok());
    let commit_oid = head.stdout.trim();

    let checkout = grit(&["checkout", commit_oid], &dir);
    assert!(
        checkout.ok(),
        "checkout of commit with '-' file failed: {}",
        checkout.stderr
    );
    assert!(dir.join("-").is_file(), "checked-out file '-' missing");
}

#[test]
fn add_dot_slash_colon_bang_filename_does_not_exclude_other_files() {
    let dir = unique_tmp("add-colon-bang");
    assert!(grit(&["init"], &dir).ok(), "init failed");

    std::fs::write(dir.join(":!foo"), "x\n").expect("create :!foo");
    std::fs::write(dir.join("other"), "y\n").expect("create other");

    let add = grit(&["add", "./:!foo"], &dir);
    assert!(add.ok(), "add ./:!foo failed: {}", add.stderr);

    let cached = grit(&["diff", "--cached", "--name-only"], &dir);
    assert!(cached.ok(), "diff --cached failed: {}", cached.stderr);
    let names: Vec<&str> = cached.stdout.lines().collect();
    assert_eq!(
        names,
        vec![":!foo"],
        "expected only literal :!foo staged, got: {:?}",
        names
    );
}
