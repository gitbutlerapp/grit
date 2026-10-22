//! `--ignored`, `--short`, and `stash_count` on `grit status`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use grit_test_support::configure_git_env;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn grit_cmd() -> Command {
    let mut cmd = Command::new(GRIT);
    configure_git_env(&mut cmd);
    cmd
}

fn git_cmd() -> Command {
    let mut cmd = Command::new("git");
    configure_git_env(&mut cmd);
    cmd
}

fn grit_json(repo: &Path, args: &[&str]) -> serde_json::Value {
    let out = grit_cmd()
        .arg("--json")
        .args(args)
        .current_dir(repo)
        .output()
        .expect("grit");
    assert!(
        out.status.success(),
        "grit {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn git_ok(repo: &Path, args: &[&str]) {
    let out = git_cmd()
        .args(args)
        .current_dir(repo)
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {:?} failed:\n{}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn status_ignored_json_lists_ignored_paths() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    fs::write(root.join(".gitignore"), "*.ignored\n")?;
    fs::write(root.join("tracked.txt"), b"ok\n")?;
    fs::write(root.join("secret.ignored"), b"x\n")?;
    git_ok(root, &["add", "tracked.txt"]);
    git_ok(root, &["commit", "-qm", "seed"]);

    let without = grit_json(root, &["status"]);
    assert!(without.get("ignored").is_none());

    let with = grit_json(root, &["status", "--ignored"]);
    let ignored = with["ignored"]
        .as_array()
        .expect("ignored array")
        .iter()
        .map(|v| v.as_str().expect("path").to_owned())
        .collect::<Vec<_>>();
    assert!(
        ignored.iter().any(|p| p == "secret.ignored"),
        "expected secret.ignored in {ignored:?}"
    );
    Ok(())
}

#[test]
fn status_ignored_human_lists_paths_on_clean_tree() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    fs::write(root.join(".gitignore"), "*.ignored\n")?;
    fs::write(root.join("tracked.txt"), b"ok\n")?;
    fs::write(root.join("secret.ignored"), b"x\n")?;
    git_ok(root, &["add", "tracked.txt"]);
    git_ok(root, &["commit", "-qm", "seed"]);

    let out = grit_cmd()
        .args(["status", "--ignored"])
        .current_dir(root)
        .output()?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("Ignored") && stdout.contains("secret.ignored"),
        "human status must list ignored paths on a clean tree:\n{stdout}"
    );
    Ok(())
}

#[test]
fn status_ignored_with_uno_still_lists_ignored_in_json() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    fs::write(root.join(".gitignore"), "*.ignored\n")?;
    fs::write(root.join("tracked.txt"), b"ok\n")?;
    fs::write(root.join("secret.ignored"), b"x\n")?;
    git_ok(root, &["add", "tracked.txt"]);
    git_ok(root, &["commit", "-qm", "seed"]);

    let v = grit_json(root, &["status", "--untracked=no", "--ignored"]);
    let ignored = v["ignored"]
        .as_array()
        .expect("ignored array")
        .iter()
        .map(|p| p.as_str().expect("path"))
        .collect::<Vec<_>>();
    assert!(
        ignored.contains(&"secret.ignored"),
        "ignored must be collected with --untracked=no: {ignored:?}"
    );
    assert!(
        v["untracked"].as_array().expect("untracked").is_empty(),
        "untracked must stay empty with --untracked=no"
    );
    Ok(())
}

#[test]
fn status_short_rename_from_subdirectory() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    fs::create_dir_all(root.join("sub"))?;
    fs::write(root.join("a -> b.txt"), b"a\n")?;
    git_ok(root, &["add", "a -> b.txt"]);
    git_ok(root, &["commit", "-qm", "seed"]);
    git_ok(root, &["mv", "a -> b.txt", "c -> d.txt"]);

    let out = grit_cmd()
        .args(["status", "--short"])
        .current_dir(root.join("sub"))
        .output()?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("../a -> b.txt -> ../c -> d.txt"),
        "each rename endpoint must be quoted separately:\n{stdout}"
    );
    assert!(
        !stdout.contains("../a -> ../b.txt -> c"),
        "must not split on arrows inside filenames:\n{stdout}"
    );
    Ok(())
}

#[test]
fn status_short_human_snapshot() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    fs::write(root.join("a.txt"), b"v1\n")?;
    git_ok(root, &["add", "a.txt"]);
    git_ok(root, &["commit", "-qm", "seed"]);
    fs::write(root.join("a.txt"), b"v2\n")?;
    fs::write(root.join("b.txt"), b"new\n")?;

    let out = grit_cmd()
        .args(["status", "--short"])
        .current_dir(root)
        .output()?;
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    insta::assert_snapshot!(stdout, @r"
    .M a.txt
    ?? b.txt
    ");
    Ok(())
}

#[test]
fn status_stash_count_after_git_stash_push() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    git_ok(root, &["init", "-q", "-b", "main", "."]);
    git_ok(root, &["config", "user.email", "t@example.com"]);
    git_ok(root, &["config", "user.name", "Test"]);
    fs::write(root.join("f.txt"), b"one\n")?;
    git_ok(root, &["add", "f.txt"]);
    git_ok(root, &["commit", "-qm", "seed"]);
    fs::write(root.join("f.txt"), b"two\n")?;
    git_ok(root, &["stash", "push", "-m", "first"]);
    fs::write(root.join("f.txt"), b"three\n")?;
    git_ok(root, &["stash", "push", "-m", "second"]);

    let v = grit_json(root, &["status"]);
    assert_eq!(v["stash_count"].as_u64(), Some(2));

    let human = grit_cmd().args(["status"]).current_dir(root).output()?;
    assert!(human.status.success());
    let stdout = String::from_utf8_lossy(&human.stdout);
    assert!(
        stdout.contains("2 stash entries"),
        "expected stash footer in human output:\n{stdout}"
    );
    Ok(())
}
