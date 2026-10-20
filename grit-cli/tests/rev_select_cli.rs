//! `grit log` / `grit revs` revision selection vs system `git`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::Command;

use serde_json::Value;

type TestResult = Result<(), Box<dyn Error>>;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

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

fn git_oids(dir: &Path, git_args: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C", dir.to_str().expect("utf8"), "log", "--format=%H"];
    argv.extend(git_args);
    let out = Command::new("git")
        .args(&argv)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git log");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
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

fn seed_linear_repo(dir: &Path, n: usize) {
    fs::create_dir_all(dir).expect("mkdir");
    assert!(grit_run(dir, &["init", "."]).status.success());
    for i in 0..n {
        fs::write(dir.join("f.txt"), format!("v{i}\n")).expect("write");
        assert!(grit_run(dir, &["add", "f.txt"]).status.success());
        assert!(grit_run(dir, &["commit", &format!("c{i}")])
            .status
            .success());
    }
}

fn seed_diverged_repo(dir: &Path) {
    seed_linear_repo(dir, 3);
    assert!(grit_run(dir, &["branch", "side"]).status.success());
    assert!(grit_run(dir, &["switch", "side"]).status.success());
    fs::write(dir.join("side.txt"), "side\n").expect("write");
    assert!(grit_run(dir, &["add", "side.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "side-tip"]).status.success());
    assert!(grit_run(dir, &["switch", "main"]).status.success());
    fs::write(dir.join("main.txt"), "main\n").expect("write");
    assert!(grit_run(dir, &["add", "main.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "main-tip"]).status.success());
    assert!(grit_run(dir, &["merge", "side"]).status.success());
}

#[test]
fn log_range_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-range-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let git = git_oids(&dir, &["main..HEAD"]);
    let grit = grit_log_oids(&dir, &["log", "-n", "50", "main..HEAD"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_since_until_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-since-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 5);
    let git = git_oids(&dir, &["--since=1", "-n", "3"]);
    let grit = grit_log_oids(&dir, &["log", "--since=1", "-n", "3"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_before_after_date_aliases() -> TestResult {
    let dir =
        std::env::temp_dir().join(format!("grit-rev-select-date-alias-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 4);
    let git = git_oids(&dir, &["--after=1", "--before=9999999999", "-n", "2"]);
    let grit = grit_log_oids(
        &dir,
        &["log", "--after=1", "--before=9999999999", "-n", "2"],
    );
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn log_before_cursor_deprecated_alias_warns() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-warn-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 12);
    let page1 = grit_json(&dir, &["log"]);
    let cursor = page1["next"].as_str().expect("next").to_owned();
    let out = grit_run(&dir, &["log", &format!("--before={cursor}")]);
    assert!(out.status.success());
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("deprecated") && stderr.contains("--cursor"),
        "stderr: {stderr}"
    );
    Ok(())
}

#[test]
fn log_cursor_pages_through_range() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-cursor-range-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 3);
    assert!(grit_run(&dir, &["branch", "base"]).status.success());
    for i in 0..12 {
        fs::write(dir.join("f.txt"), format!("main{i}\n")).expect("write");
        assert!(grit_run(&dir, &["add", "f.txt"]).status.success());
        assert!(grit_run(&dir, &["commit", &format!("m{i}")])
            .status
            .success());
    }
    let git = git_oids(&dir, &["base..HEAD"]);
    let page1 = grit_json(&dir, &["log", "base..HEAD"]);
    assert_eq!(page1["commits"].as_array().expect("commits").len(), 10);
    let cursor = page1["next"].as_str().expect("next cursor for range walk");
    let page2 = grit_json(&dir, &["log", &format!("--cursor={cursor}")]);
    assert!(page2["next"].is_null());
    let paged: Vec<String> = page1["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .chain(page2["commits"].as_array().expect("commits").iter())
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect();
    assert_eq!(paged, git);
    Ok(())
}

#[test]
fn revs_left_right_count_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-lr-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let out = Command::new("git")
        .args([
            "-C",
            dir.to_str().expect("utf8"),
            "rev-list",
            "--left-right",
            "--count",
            "main...side",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()?;
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    let mut parts = stdout.split_whitespace();
    let git_left: usize = parts.next().unwrap_or("0").parse()?;
    let git_right: usize = parts.next().unwrap_or("0").parse()?;
    let v = grit_json(&dir, &["revs", "--count", "--left-right", "main...side"]);
    assert_eq!(v["left_right"]["left"], git_left);
    assert_eq!(v["left_right"]["right"], git_right);
    assert!(v["count"].is_null());
    Ok(())
}

#[test]
fn revs_all_branches_tags_remotes_match_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-refs-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    assert!(grit_run(&dir, &["tag", "t1"]).status.success());
    for (flag, grit_flag) in [
        ("--all", "--all"),
        ("--branches", "--branches"),
        ("--tags", "--tags"),
    ] {
        let git = git_rev_list_oids(&dir, &[flag]);
        let grit = grit_revs_oids(&dir, &["revs", grit_flag]);
        assert_eq!(grit, git, "{flag}");
    }
    Ok(())
}

#[test]
fn limit_hidden_alias_still_works() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-limit-alias-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 6);
    let git = git_oids(&dir, &["-n", "3"]);
    let grit = grit_log_oids(&dir, &["log", "--limit", "3"]);
    assert_eq!(grit, git);
    Ok(())
}

#[test]
fn revs_n_zero_left_right_json_shape() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-rev-select-lr-zero-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let v = grit_json(
        &dir,
        &["revs", "--count", "--left-right", "-n", "0", "main...side"],
    );
    assert!(v["count"].is_null());
    assert_eq!(v["left_right"]["left"], 0);
    assert_eq!(v["left_right"]["right"], 0);
    Ok(())
}

#[test]
fn merges_conflicts_with_no_merges() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-rev-select-merge-conflict-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 2);
    let out = grit_run(&dir, &["revs", "HEAD", "--merges", "--no-merges"]);
    assert_eq!(out.status.code(), Some(2));
    Ok(())
}

fn git_rev_list_oids(dir: &Path, extra: &[&str]) -> Vec<String> {
    let mut argv = vec!["-C", dir.to_str().expect("utf8"), "rev-list"];
    argv.extend(extra);
    let out = Command::new("git")
        .args(&argv)
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git rev-list");
    assert!(out.status.success());
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect()
}

fn grit_revs_oids(dir: &Path, args: &[&str]) -> Vec<String> {
    let v = grit_json(dir, args);
    v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["oid"].as_str().expect("oid").to_owned())
        .collect()
}
