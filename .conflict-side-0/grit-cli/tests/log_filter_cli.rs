//! `grit log` / `grit revs` commit filters vs system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");
const INVALID_FILTER_EXIT: i32 = 129;

fn null_device() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn grit_json(dir: &Path, args: &[&str]) -> Value {
    let out = Command::new(GRIT)
        .arg("--json")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit");
    assert_eq!(
        out.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

fn grit_run(dir: &Path, args: &[&str]) -> std::process::Output {
    Command::new(GRIT)
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("spawn grit")
}

fn git(dir: &Path, args: &[&str]) -> String {
    let mut argv = vec!["-C", dir.to_str().expect("utf8")];
    argv.extend(args);
    let out = Command::new("git")
        .args(&argv)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).to_string()
}

fn git_log_oids(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut args = vec!["log", "--format=%H"];
    args.extend(extra);
    git(dir, &args)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_log_oids(dir: &Path, args: &[&str]) -> Vec<String> {
    let v = grit_json(dir, args);
    v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect()
}

fn commit_tree_with_identities(
    dir: &Path,
    label: &str,
    parents: &[&str],
    author: (&str, &str),
    message: &str,
) -> String {
    let tree = git(dir, &["mktree"]).trim().to_string();
    let mut args = vec!["commit-tree", &tree, "-m", message];
    for p in parents {
        args.push("-p");
        args.push(p);
    }
    let out = Command::new("git")
        .current_dir(dir)
        .args(&args)
        .env("GIT_AUTHOR_NAME", author.0)
        .env("GIT_AUTHOR_EMAIL", author.1)
        .env("GIT_COMMITTER_NAME", author.0)
        .env("GIT_COMMITTER_EMAIL", author.1)
        .env("GIT_AUTHOR_DATE", "@1000 +0000")
        .env("GIT_COMMITTER_DATE", "@1000 +0000")
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("commit-tree");
    assert!(out.status.success());
    let commit = String::from_utf8_lossy(&out.stdout).trim().to_string();
    git(
        dir,
        &["update-ref", &format!("refs/history/{label}"), &commit],
    );
    commit
}

fn seed_filter_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    assert!(grit_run(dir, &["init", "."]).status.success());
    let mut prev = commit_tree_with_identities(dir, "c0", &[], ("Alice", "a@x.com"), "alpha one");
    prev = commit_tree_with_identities(dir, "c1", &[&prev], ("Bob", "b@x.com"), "beta JIRA-123");
    prev = commit_tree_with_identities(dir, "c2", &[&prev], ("Ada", "ada@x.com"), "gamma JIRA-456");
    prev = commit_tree_with_identities(dir, "c3", &[&prev], ("Ada", "ada@x.com"), "delta tip");
    git(dir, &["update-ref", "refs/heads/main", &prev]);
}

#[test]
fn log_author_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-author-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    let git = git_log_oids(&dir, &["--author=Ada", "-n", "20"]);
    let grit = grit_log_oids(&dir, &["log", "--author=Ada", "-n", "20"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_grep_all_match_invert_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-grep-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    for (extra_git, extra_grit) in [
        (
            vec!["--grep=JIRA", "--grep=456", "--all-match", "-n", "20"],
            vec![
                "log",
                "--grep=JIRA",
                "--grep=456",
                "--all-match",
                "-n",
                "20",
            ],
        ),
        (
            vec!["--grep=JIRA", "--invert-grep", "-n", "20"],
            vec!["log", "--grep=JIRA", "--invert-grep", "-n", "20"],
        ),
    ] {
        let git = git_log_oids(&dir, &extra_git);
        let grit = grit_log_oids(&dir, &extra_grit);
        assert_eq!(grit, git, "git={extra_git:?} grit={extra_grit:?}");
    }
    Ok(())
}

#[test]
fn log_ignore_case_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-i-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    let git = git_log_oids(&dir, &["--author=ada", "-i", "-n", "20"]);
    let grit = grit_log_oids(&dir, &["log", "--author=ada", "-i", "-n", "20"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_fixed_strings() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-F-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    let git = git_log_oids(&dir, &["-F", "--grep=JIRA-123", "-n", "20"]);
    let grit = grit_log_oids(&dir, &["log", "-F", "--grep=JIRA-123", "-n", "20"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_author_mailmap_and_no_mailmap_match_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-mailmap-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    fs::write(
        dir.join(".mailmap"),
        "Proper Name <proper@email.com> <old@email.com>\n",
    )?;
    git(&dir, &["config", "log.mailmap", "true"]);
    let tip = commit_tree_with_identities(
        &dir,
        "mapped",
        &[],
        ("Wrong", "old@email.com"),
        "mapped commit",
    );
    git(&dir, &["update-ref", "refs/heads/main", &tip]);
    let git_with = git_log_oids(
        &dir,
        &["--use-mailmap", "--author", "Proper Name", "-n", "5"],
    );
    let grit_with = grit_log_oids(&dir, &["log", "--author", "Proper Name", "-n", "5"]);
    assert_eq!(grit_with, git_with);
    let git_without = git(
        &dir,
        &[
            "-c",
            "log.mailmap=false",
            "log",
            "--format=%H",
            "--author",
            "Wrong",
            "-n",
            "5",
        ],
    )
    .lines()
    .filter(|l| !l.is_empty())
    .map(str::to_owned)
    .collect::<Vec<_>>();
    let grit_without = grit_log_oids(&dir, &["log", "--author=Wrong", "--no-mailmap", "-n", "5"]);
    assert_eq!(grit_without, git_without);
    Ok(())
}

#[test]
fn log_n_counts_filtered_commits() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-n-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    let git = git_log_oids(&dir, &["--author=Ada", "-n", "1"]);
    let grit = grit_log_oids(&dir, &["log", "--author=Ada", "-n", "1"]);
    assert_eq!(grit.len(), 1);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn revs_count_author_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-filter-count-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    let git_n: usize = git(&dir, &["rev-list", "--count", "--author=Ada", "main"])
        .trim()
        .parse()?;
    let v = grit_json(&dir, &["revs", "--count", "--author=Ada", "main"]);
    assert_eq!(v["count"], git_n);
    Ok(())
}

#[test]
fn log_invalid_pattern_errors_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-log-filter-bad-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_filter_repo(&dir);
    let out = Command::new(GRIT)
        .args(["--json", "log", "--author", "["])
        .current_dir(&dir)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert_eq!(out.status.code(), Some(INVALID_FILTER_EXIT));
    let v: Value = serde_json::from_slice(&out.stdout)?;
    insta::assert_json_snapshot!(v, @r#"
    {
      "error": "invalid commit filter pattern `[`: unterminated bracket expression",
      "kind": "invalid_pattern"
    }
    "#);
    Ok(())
}
