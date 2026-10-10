use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit(dir: &Path, args: &[&str]) -> (i32, String, String) {
    let out = Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "Test User")
        .env("GIT_AUTHOR_EMAIL", "test@example.com")
        .env("GIT_COMMITTER_NAME", "Test User")
        .env("GIT_COMMITTER_EMAIL", "test@example.com")
        .output()
        .expect("grit");
    (
        out.status.code().unwrap_or(-1),
        String::from_utf8_lossy(&out.stdout).into_owned(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

fn init_repo(dir: &Path) {
    git(dir, &["init", "-q", "-b", "main", "."]);
    git(dir, &["config", "user.email", "test@example.com"]);
    git(dir, &["config", "user.name", "Test User"]);
}

fn scratch(tag: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("grit-stash-test-{}-{}", tag, std::process::id()));
    let _ = fs::remove_dir_all(&path);
    fs::create_dir_all(&path).expect("mkdir");
    path
}

#[test]
fn stash_push_pop_roundtrip() {
    let dir = scratch("roundtrip");
    init_repo(&dir);
    fs::write(dir.join("f.txt"), "base\n").expect("write");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "init"]);

    fs::write(dir.join("f.txt"), "changed\n").expect("modify");
    let (code, stdout, stderr) = grit(&dir, &["stash", "push", "-m", "wip"]);
    assert_eq!(code, 0, "stderr={stderr}");
    assert!(stdout.contains("Saved") || stderr.is_empty());

    let porcelain = git(&dir, &["status", "--porcelain"]);
    assert!(porcelain.trim().is_empty(), "clean after push");

    let (code, _, stderr) = grit(&dir, &["stash", "pop"]);
    assert_eq!(code, 0, "pop failed: {stderr}");
    assert_eq!(
        fs::read_to_string(dir.join("f.txt")).expect("read"),
        "changed\n"
    );
    let list = git(&dir, &["stash", "list"]);
    assert!(list.trim().is_empty(), "stash should be empty after pop");
}

#[test]
fn stash_list_json() {
    let dir = scratch("list-json");
    init_repo(&dir);
    fs::write(dir.join("a"), "v\n").expect("write");
    git(&dir, &["add", "a"]);
    git(&dir, &["commit", "-qm", "init"]);
    fs::write(dir.join("a"), "one\n").expect("one");
    grit(&dir, &["stash", "-m", "first"]);
    fs::write(dir.join("a"), "two\n").expect("two");
    grit(&dir, &["stash", "-m", "second"]);

    let (code, stdout, _) = grit(&dir, &["stash", "list", "--json"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    let entries = parsed["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0]["index"], 0);
    assert!(entries[0]["oid"].as_str().unwrap().len() >= 40);
    assert!(entries[0]["message"].as_str().unwrap().contains("second"));
}

#[test]
fn stash_pop_conflict_keeps_entry() {
    let dir = scratch("conflict");
    init_repo(&dir);
    fs::write(dir.join("f.txt"), "base\n").expect("write");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "init"]);

    fs::write(dir.join("f.txt"), "from-stash\n").expect("stash me");
    grit(&dir, &["stash"]);
    fs::write(dir.join("f.txt"), "on-branch\n").expect("branch edit");
    git(&dir, &["add", "f.txt"]);
    git(&dir, &["commit", "-qm", "second"]);

    let (code, stdout, stderr) = grit(&dir, &["stash", "pop"]);
    assert_ne!(code, 0, "pop with conflicts must fail");
    let combined = format!("{stdout}{stderr}");
    assert!(
        combined.contains("conflict") || combined.contains("kept"),
        "output should mention conflict/kept: {combined}"
    );

    let list = git(&dir, &["stash", "list"]);
    assert!(
        !list.trim().is_empty(),
        "stash entry must remain after conflicted pop"
    );
}

#[test]
fn stash_interop_with_git() {
    let dir = scratch("interop");
    init_repo(&dir);
    fs::write(dir.join("x"), "v1\n").expect("write");
    git(&dir, &["add", "x"]);
    git(&dir, &["commit", "-qm", "init"]);

    fs::write(dir.join("x"), "grit-stash\n").expect("modify");
    grit(&dir, &["stash", "-m", "from-grit"]);
    assert!(git(&dir, &["status", "--porcelain"]).trim().is_empty());

    git(&dir, &["stash", "pop"]);
    assert_eq!(
        fs::read_to_string(dir.join("x")).expect("read"),
        "grit-stash\n"
    );

    fs::write(dir.join("x"), "git-stash\n").expect("modify");
    git(&dir, &["stash", "push", "-m", "from-git"]);

    let (code, stdout, _) = grit(&dir, &["stash", "list", "--json"]);
    assert_eq!(code, 0);
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim()).expect("json");
    assert_eq!(parsed["entries"].as_array().unwrap().len(), 1);

    let (code, _, _) = grit(&dir, &["stash", "pop"]);
    assert_eq!(code, 0);
    assert_eq!(
        fs::read_to_string(dir.join("x")).expect("read"),
        "git-stash\n"
    );
}
