//! `grit add -u` and `--dry-run` CLI wiring and JSON shape.

use std::fs;
use std::process::Command;

use serde_json::Value;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn run_grit(dir: &std::path::Path, args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_grit"))
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run grit")
}

fn git_out(dir: &std::path::Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(dir)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

fn json(dir: &std::path::Path, args: &[&str]) -> Value {
    let out = run_grit(dir, args);
    assert!(
        out.status.success(),
        "grit {:?}: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

#[test]
fn add_update_skips_untracked_json() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let repo = dir.path();
    assert!(run_grit(repo, &["init"]).status.success());
    git_out(repo, &["config", "user.email", "t@example.com"]);
    git_out(repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("tracked.txt"), b"old\n").unwrap();
    git_out(repo, &["add", "tracked.txt"]);
    git_out(repo, &["commit", "-qm", "seed"]);
    fs::write(repo.join("tracked.txt"), b"new\n").unwrap();
    fs::write(repo.join("new.txt"), b"hi\n").unwrap();

    let v = json(repo, &["add", "-u", "--json"]);
    assert_eq!(v["count"], 1);
    assert_eq!(v["staged"], 1);
    assert_eq!(v["modified"].as_array().unwrap(), &["tracked.txt"]);
    assert!(v["added"].as_array().unwrap().is_empty());
    assert_eq!(v["dry_run"], false);

    let ls = git_out(repo, &["ls-files"]);
    assert!(
        !ls.contains("new.txt"),
        "untracked must stay unstaged: {ls}"
    );
}

#[test]
fn add_dry_run_json_lists_paths_without_index_write() {
    let dir = tempfile::TempDir::new().expect("tempdir");
    let repo = dir.path();
    assert!(run_grit(repo, &["init"]).status.success());
    git_out(repo, &["config", "user.email", "t@example.com"]);
    git_out(repo, &["config", "user.name", "Test"]);
    fs::write(repo.join("a.txt"), b"a\n").unwrap();
    git_out(repo, &["add", "a.txt"]);
    git_out(repo, &["commit", "-qm", "seed"]);
    fs::write(repo.join("a.txt"), b"b\n").unwrap();
    fs::write(repo.join("n.txt"), b"n\n").unwrap();
    let index_mtime_before = fs::metadata(repo.join(".git/index"))
        .unwrap()
        .modified()
        .unwrap();

    let v = json(repo, &["add", "--dry-run", "--json"]);
    assert_eq!(v["dry_run"], true);
    assert!(v["modified"]
        .as_array()
        .unwrap()
        .contains(&Value::from("a.txt")));
    assert!(v["added"]
        .as_array()
        .unwrap()
        .contains(&Value::from("n.txt")));
    assert!(v["count"].as_u64().unwrap() >= 2);

    let index_mtime_after = fs::metadata(repo.join(".git/index"))
        .unwrap()
        .modified()
        .unwrap();
    assert_eq!(index_mtime_before, index_mtime_after);
    let ls = git_out(repo, &["ls-files"]);
    assert!(!ls.contains("n.txt"));
}
