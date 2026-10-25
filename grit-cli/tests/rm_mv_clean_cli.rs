//! CLI JSON for `grit rm`, `grit mv`, and `grit clean` new flags.

use std::process::Command;

use grit_test_support::git;
use serde_json::Value;

fn grit_json(dir: &std::path::Path, args: &[&str]) -> Value {
    let mut full = vec!["--json"];
    full.extend_from_slice(args);
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(&full)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?}: {}",
        full,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn rm_dry_run_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("t.txt"), b"x\n").unwrap();
    git(dir.path(), &["add", "t.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    let before = std::fs::read(dir.path().join(".git/index")).unwrap();
    let v = grit_json(dir.path(), &["rm", "--dry-run", "t.txt"]);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["removed"][0], "t.txt");
    assert_eq!(
        std::fs::read(dir.path().join(".git/index")).unwrap(),
        before
    );
    assert!(dir.path().join("t.txt").is_file());
}

fn grit_failure(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("grit");
    assert_eq!(out.status.code(), Some(1), "grit {args:?} should fail");
    String::from_utf8_lossy(&out.stderr).into_owned()
}

#[test]
fn rm_cached_keeps_committed_file_edited_since() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join(".env"), b"secret\n").unwrap();
    git(dir.path(), &["add", ".env"]);
    git(dir.path(), &["commit", "-q", "-m", "oops"]);
    std::fs::write(dir.path().join(".env"), b"secret\nmore\n").unwrap();

    let v = grit_json(dir.path(), &["rm", "--cached", ".env"]);
    assert_eq!(v["removed"][0], ".env");
    assert_eq!(
        std::fs::read(dir.path().join(".env")).unwrap(),
        b"secret\nmore\n"
    );
}

#[test]
fn rm_refusal_hint_never_suggests_cached_when_it_cannot_help() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("local.txt"), b"v1\n").unwrap();
    std::fs::write(dir.path().join("both.txt"), b"v1\n").unwrap();
    git(dir.path(), &["add", "."]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    std::fs::write(dir.path().join("local.txt"), b"v2\n").unwrap();
    std::fs::write(dir.path().join("both.txt"), b"v2\n").unwrap();
    git(dir.path(), &["add", "both.txt"]);
    std::fs::write(dir.path().join("both.txt"), b"v3\n").unwrap();

    let local = grit_failure(dir.path(), &["rm", "local.txt"]);
    assert!(local.contains("local.txt (local modifications)"), "{local}");
    assert!(local.contains("--cached"), "{local}");
    assert!(local.contains("--force"), "{local}");

    let cached = grit_failure(dir.path(), &["rm", "--cached", "both.txt"]);
    assert!(
        cached.contains("both.txt (staged content differs from both HEAD and the working tree)"),
        "{cached}"
    );
    assert!(!cached.contains("--cached"), "{cached}");
    assert!(cached.contains("--force"), "{cached}");

    let mixed = grit_failure(dir.path(), &["rm", "local.txt", "both.txt"]);
    assert!(!mixed.contains("--cached"), "{mixed}");
}

#[test]
fn rm_ignore_unmatch_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("k.txt"), b"k\n").unwrap();
    git(dir.path(), &["add", "k.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    let v = grit_json(dir.path(), &["rm", "--ignore-unmatch", "missing.txt"]);
    assert_eq!(v["removed"].as_array().unwrap().len(), 0);
}

#[test]
fn mv_dry_run_json_moves_array() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join("a.txt"), b"a\n").unwrap();
    git(dir.path(), &["add", "a.txt"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    let v = grit_json(dir.path(), &["mv", "--dry-run", "a.txt", "b.txt"]);
    assert_eq!(v["dry_run"], true);
    assert_eq!(v["moves"][0]["from"], "a.txt");
    assert_eq!(v["moves"][0]["to"], "b.txt");
    assert!(dir.path().join("a.txt").is_file());
}

#[test]
fn clean_scope_and_files_only_json() {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main"]);
    std::fs::write(dir.path().join(".gitignore"), b"*.ig\n").unwrap();
    std::fs::write(dir.path().join("x.ig"), b"i\n").unwrap();
    std::fs::write(dir.path().join("u.txt"), b"u\n").unwrap();
    git(dir.path(), &["add", ".gitignore"]);
    git(dir.path(), &["commit", "-q", "-m", "init"]);
    let only = grit_json(dir.path(), &["clean", "--only-ignored"]);
    assert_eq!(only["scope"], "only_ignored");
    assert!(only["removed"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p.as_str() == Some("x.ig")));
    let files = grit_json(dir.path(), &["clean", "--files-only"]);
    assert_eq!(files["scope"], "untracked");
}
