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
