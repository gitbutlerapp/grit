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

fn seed_cherry_equivalent_repo(dir: &Path) {
    fs::create_dir_all(dir).expect("mkdir");
    assert!(grit_run(dir, &["init", "."]).status.success());
    fs::write(dir.join("shared.txt"), "origin\n").expect("write");
    assert!(grit_run(dir, &["add", "shared.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "seed"]).status.success());
    let rename = Command::new("git")
        .args(["-C", dir.to_str().expect("utf8"), "branch", "-M", "main"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("git branch -M");
    assert!(rename.success());
    fs::write(dir.join("shared.txt"), "same\n").expect("write");
    assert!(grit_run(dir, &["add", "shared.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "left-patch"]).status.success());
    let base_out = Command::new("git")
        .args(["-C", dir.to_str().expect("utf8"), "rev-parse", "HEAD~1"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git rev-parse");
    assert!(base_out.status.success());
    let base = String::from_utf8_lossy(&base_out.stdout).trim().to_owned();
    assert!(grit_run(dir, &["switch", "-c", "side", &base])
        .status
        .success());
    fs::write(dir.join("side.txt"), "side\n").expect("write");
    assert!(grit_run(dir, &["add", "side.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "side-only"]).status.success());
    fs::write(dir.join("shared.txt"), "same\n").expect("write");
    assert!(grit_run(dir, &["add", "shared.txt"]).status.success());
    assert!(grit_run(dir, &["commit", "right-patch"]).status.success());
    assert!(grit_run(dir, &["switch", "main"]).status.success());
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
        let mut git = git_rev_list_oids(&dir, &[flag]);
        let mut grit = grit_revs_oids(&dir, &["revs", grit_flag]);
        if flag == "--all" {
            // Multiple ref tips can share committer time; Git and grit may tie-break differently.
            git.sort();
            grit.sort();
        }
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
fn revs_disconnected_left_right_count_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-disc-lr-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).expect("mkdir");
    assert!(grit_run(&dir, &["init", "."]).status.success());
    fs::write(dir.join("left.txt"), "left\n").expect("write");
    assert!(grit_run(&dir, &["add", "left.txt"]).status.success());
    assert!(grit_run(&dir, &["commit", "left-root"]).status.success());
    let rename = Command::new("git")
        .args(["-C", dir.to_str().expect("utf8"), "branch", "-M", "main"])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("git branch -M");
    assert!(rename.success());
    assert!(Command::new("git")
        .args([
            "-C",
            dir.to_str().expect("utf8"),
            "checkout",
            "--orphan",
            "side"
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .status()
        .expect("orphan")
        .success());
    fs::write(dir.join("right.txt"), "right\n").expect("write");
    assert!(grit_run(&dir, &["add", "right.txt"]).status.success());
    assert!(grit_run(&dir, &["commit", "right-root"]).status.success());
    assert!(grit_run(&dir, &["switch", "main"]).status.success());
    for cherry in [false, true] {
        let git_out = if cherry {
            Command::new("git")
                .args([
                    "-C",
                    dir.to_str().expect("utf8"),
                    "rev-list",
                    "--left-right",
                    "--cherry-pick",
                    "--count",
                    "main...side",
                ])
                .env("GIT_CONFIG_GLOBAL", null_device())
                .env("GIT_CONFIG_SYSTEM", null_device())
                .output()
                .expect("git count")
        } else {
            Command::new("git")
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
                .output()
                .expect("git count")
        };
        assert!(git_out.status.success());
        let git_stdout = String::from_utf8_lossy(&git_out.stdout);
        let parts: Vec<&str> = git_stdout.split_whitespace().collect();
        let git_left: u64 = parts[0].parse().unwrap();
        let git_right: u64 = parts[1].parse().unwrap();
        let v = if cherry {
            grit_json(
                &dir,
                &[
                    "revs",
                    "--count",
                    "--left-right",
                    "--cherry-pick",
                    "main...side",
                ],
            )
        } else {
            grit_json(&dir, &["revs", "--count", "--left-right", "main...side"])
        };
        assert_eq!(v["left_right"]["left"], git_left);
        assert_eq!(v["left_right"]["right"], git_right);
    }
    Ok(())
}

#[test]
fn revs_json_omits_parents_without_flag() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-json-parents-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 3);
    let v = grit_json(&dir, &["revs", "HEAD", "-n", "2"]);
    for row in v["commits"].as_array().expect("commits") {
        assert!(
            row["parents"].as_array().expect("parents").is_empty(),
            "parents without --parents: {row:?}"
        );
    }
    Ok(())
}

#[test]
fn revs_parents_limit_with_path_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-n-parents-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 1);
    fs::write(dir.join("track.txt"), "a\n").expect("write");
    assert!(grit_run(&dir, &["add", "track.txt"]).status.success());
    assert!(grit_run(&dir, &["commit", "touch"]).status.success());
    fs::write(dir.join("other.txt"), "b\n").expect("write");
    assert!(grit_run(&dir, &["add", "other.txt"]).status.success());
    assert!(grit_run(&dir, &["commit", "noise"]).status.success());
    let git_out = Command::new("git")
        .args([
            "-C",
            dir.to_str().expect("utf8"),
            "rev-list",
            "-n",
            "1",
            "--parents",
            "HEAD",
            "--",
            "track.txt",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git");
    assert!(git_out.status.success());
    let git_line = String::from_utf8_lossy(&git_out.stdout).trim().to_owned();
    let git_tokens: Vec<_> = git_line.split_whitespace().collect();
    let out = grit_run(
        &dir,
        &["revs", "-n", "1", "--parents", "HEAD", "--", "track.txt"],
    );
    assert!(out.status.success());
    let grit_line = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    let grit_oid_part = grit_line.split("  ").next().expect("oid part");
    let grit_tokens: Vec<_> = grit_oid_part.split_whitespace().collect();
    assert_eq!(grit_tokens, git_tokens);
    Ok(())
}

#[test]
fn revs_plain_human_omits_parents() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-plain-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_linear_repo(&dir, 3);
    let out = grit_run(&dir, &["revs", "HEAD", "-n", "2"]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let parts: Vec<_> = line.split("  ").collect();
        assert_eq!(parts.len(), 2, "expected oid + subject only: {line}");
        let oid_part = parts[0];
        assert_eq!(
            oid_part.split_whitespace().count(),
            1,
            "unexpected extra tokens before subject: {line}"
        );
    }
    Ok(())
}

#[test]
fn revs_cherry_pick_without_left_right_matches_git() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-cherry-no-lr-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let git_lines = git_rev_list_oids(&dir, &["--cherry-pick", "main...side"]);
    let out = grit_run(&dir, &["revs", "--cherry-pick", "main...side"]);
    assert!(out.status.success());
    let grit_lines: Vec<_> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(grit_lines.len(), git_lines.len());
    for (grit_line, git_line) in grit_lines.iter().zip(git_lines.iter()) {
        assert!(
            !grit_line.starts_with('<') && !grit_line.starts_with('>'),
            "side marker without --left-right: {grit_line}"
        );
        let grit_oid = grit_line.split("  ").next().expect("oid");
        let git_oid = git_line.split_whitespace().next().expect("git oid");
        assert_eq!(grit_oid, git_oid);
    }
    Ok(())
}

#[test]
fn revs_cherry_mark_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-cherry-mark-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_diverged_repo(&dir);
    let git_lines = git_rev_list_oids(&dir, &["--left-right", "--cherry-mark", "main...side"]);
    let v = grit_json(
        &dir,
        &["revs", "--left-right", "--cherry-mark", "main...side"],
    );
    let commits = v["commits"].as_array().expect("commits");
    assert_eq!(commits.len(), git_lines.len());
    for (row, git_line) in commits.iter().zip(git_lines.iter()) {
        let stripped = git_line.trim_start_matches(['<', '>', '=', ' '].as_ref());
        let git_oid = stripped.split_whitespace().next().expect("oid token");
        assert_eq!(row["oid"].as_str().expect("oid"), git_oid);
        if git_line.starts_with('=') {
            assert_eq!(row["equivalent"].as_bool(), Some(true));
        }
    }
    Ok(())
}

#[test]
fn revs_cherry_pick_left_right_count_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!("grit-revs-cherry-count-{}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    seed_cherry_equivalent_repo(&dir);
    let git_out = Command::new("git")
        .args([
            "-C",
            dir.to_str().expect("utf8"),
            "rev-list",
            "--left-right",
            "--cherry-pick",
            "--count",
            "main...side",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git count");
    assert!(git_out.status.success());
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let parts: Vec<&str> = git_stdout.split_whitespace().collect();
    let v = grit_json(
        &dir,
        &[
            "revs",
            "--count",
            "--left-right",
            "--cherry-pick",
            "main...side",
        ],
    );
    assert_eq!(v["left_right"]["left"], parts[0].parse::<u64>().unwrap());
    assert_eq!(v["left_right"]["right"], parts[1].parse::<u64>().unwrap());
    assert!(v["left_right"].get("equivalent").is_none());
    Ok(())
}

#[test]
fn revs_cherry_mark_left_right_count_json() -> TestResult {
    let dir = std::env::temp_dir().join(format!(
        "grit-revs-cherry-mark-count-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&dir);
    seed_cherry_equivalent_repo(&dir);
    let git_out = Command::new("git")
        .args([
            "-C",
            dir.to_str().expect("utf8"),
            "rev-list",
            "--left-right",
            "--cherry-mark",
            "--count",
            "main...side",
        ])
        .env("GIT_CONFIG_GLOBAL", null_device())
        .env("GIT_CONFIG_SYSTEM", null_device())
        .output()
        .expect("git count");
    assert!(git_out.status.success());
    let git_stdout = String::from_utf8_lossy(&git_out.stdout);
    let parts: Vec<&str> = git_stdout.split_whitespace().collect();
    assert_eq!(parts.len(), 3);
    let v = grit_json(
        &dir,
        &[
            "revs",
            "--count",
            "--left-right",
            "--cherry-mark",
            "main...side",
        ],
    );
    assert_eq!(v["left_right"]["left"], parts[0].parse::<u64>().unwrap());
    assert_eq!(v["left_right"]["right"], parts[1].parse::<u64>().unwrap());
    assert_eq!(
        v["left_right"]["equivalent"],
        parts[2].parse::<u64>().unwrap()
    );
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
