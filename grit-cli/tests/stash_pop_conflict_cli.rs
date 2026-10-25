//! A conflicted `grit stash pop`: next steps, `grit restore` on the unmerged path, and the
//! `grit add` + `grit stash drop` resolution, cross-checked with system `git`.

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn run(program: &str, repo: &Path, args: &[&str]) -> Output {
    Command::new(program)
        .current_dir(repo)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "T")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "T")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .unwrap_or_else(|e| panic!("{program} {args:?}: {e}"))
}

fn grit_ok(repo: &Path, args: &[&str]) -> Output {
    let out = run(GRIT, repo, args);
    assert!(
        out.status.success(),
        "grit {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

/// Stash a local edit of `f.txt` on `main`, switch to `b1` (which changed the same line) and
/// leave the repository ready for a conflicting `grit stash pop`.
fn stash_then_switch(dir: &Path) {
    grit_ok(dir, &["init", "-b", "main", "."]);
    fs::write(dir.join("f.txt"), "base\n").unwrap();
    grit_ok(dir, &["commit", "-m", "base"]);
    grit_ok(dir, &["switch", "-c", "b1"]);
    fs::write(dir.join("f.txt"), "b1\n").unwrap();
    grit_ok(dir, &["commit", "-m", "b1"]);
    grit_ok(dir, &["switch", "main"]);
    fs::write(dir.join("f.txt"), "local\n").unwrap();

    let blocked = run(GRIT, dir, &["switch", "b1"]);
    assert_eq!(blocked.status.code(), Some(1));
    assert!(stderr(&blocked).contains("stash"), "{}", stderr(&blocked));

    grit_ok(dir, &["stash"]);
    grit_ok(dir, &["switch", "b1"]);
}

#[test]
fn conflicted_stash_pop_points_at_add_and_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_then_switch(dir.path());

    let pop = run(GRIT, dir.path(), &["stash", "pop"]);
    assert_eq!(pop.status.code(), Some(1));
    let err = stderr(&pop);
    assert!(err.contains("The stash entry was kept."), "{err}");
    assert!(err.contains("grit add <file>"), "{err}");
    assert!(err.contains("grit stash drop"), "{err}");
    assert!(!err.contains("merge"), "{err}");

    let status = stdout(&grit_ok(dir.path(), &["status"]));
    assert!(!status.contains("merge --continue"), "{status}");
}

#[test]
fn conflicted_stash_pop_markdown_and_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_then_switch(dir.path());
    let md = run(GRIT, dir.path(), &["stash", "pop", "--markdown"]);
    assert_eq!(md.status.code(), Some(1));
    let md = stdout(&md);
    assert!(md.contains("(entry kept)"), "{md}");
    assert!(md.contains("`grit stash drop`"), "{md}");

    let dir = tempfile::tempdir().expect("tempdir");
    stash_then_switch(dir.path());
    let json = run(GRIT, dir.path(), &["stash", "pop", "--json"]);
    assert_eq!(json.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json");
    assert_eq!(json["dropped"], false);
    assert_eq!(json["conflicts"], true);
    assert_eq!(json["conflict_paths"], serde_json::json!(["f.txt"]));
}

#[test]
fn restore_after_conflicted_stash_pop_reports_unmerged_path() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_then_switch(dir.path());
    assert_eq!(
        run(GRIT, dir.path(), &["stash", "pop"]).status.code(),
        Some(1)
    );
    let markers = fs::read(dir.path().join("f.txt")).unwrap();

    let git_restore = run("git", dir.path(), &["restore", "f.txt"]);
    assert!(!git_restore.status.success());

    let restore = run(GRIT, dir.path(), &["restore", "f.txt"]);
    assert_eq!(restore.status.code(), Some(1));
    let err = stderr(&restore);
    assert!(err.contains("path 'f.txt' is unmerged"), "{err}");
    assert!(err.contains("--ours"), "{err}");
    assert!(!err.contains("did not match"), "{err}");
    assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), markers);

    let json = run(GRIT, dir.path(), &["--json", "restore", "f.txt"]);
    assert_eq!(json.status.code(), Some(1));
    let json: serde_json::Value = serde_json::from_slice(&json.stdout).expect("json");
    assert_eq!(json["kind"], "conflict");
    assert_eq!(json["conflicts"], serde_json::json!(["f.txt"]));

    grit_ok(dir.path(), &["restore", "--theirs", "f.txt"]);
    assert_eq!(fs::read(dir.path().join("f.txt")).unwrap(), b"local\n");
}

#[test]
fn resolving_a_conflicted_stash_pop_with_add_and_drop() {
    let dir = tempfile::tempdir().expect("tempdir");
    stash_then_switch(dir.path());
    assert_eq!(
        run(GRIT, dir.path(), &["stash", "pop"]).status.code(),
        Some(1)
    );

    fs::write(dir.path().join("f.txt"), "resolved\n").unwrap();
    grit_ok(dir.path(), &["add", "f.txt"]);
    grit_ok(dir.path(), &["stash", "drop"]);

    let unmerged = run("git", dir.path(), &["ls-files", "-u"]);
    assert!(stdout(&unmerged).trim().is_empty(), "{}", stdout(&unmerged));
    let stashes = run("git", dir.path(), &["stash", "list"]);
    assert!(stdout(&stashes).trim().is_empty(), "{}", stdout(&stashes));
    let fsck = run("git", dir.path(), &["fsck", "--no-progress"]);
    assert!(fsck.status.success(), "{}", stderr(&fsck));
    let short = run("git", dir.path(), &["status", "--short"]);
    assert_eq!(stdout(&short), "M  f.txt\n");
}
