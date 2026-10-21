//! Path-limited status, untracked modes, and rename JSON vs Git porcelain v2.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeSet;
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

fn git_porcelain_v2_paths(repo: &Path, pathspec: &[&str]) -> BTreeSet<String> {
    let mut cmd = git_cmd();
    cmd.args(["status", "--porcelain=v2", "--"]);
    cmd.args(pathspec);
    let out = cmd.current_dir(repo).output().expect("git status");
    assert!(
        out.status.success(),
        "git status porcelain v2 failed:\n{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let mut paths = BTreeSet::new();
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let path = porcelain_v2_path(line);
        if let Some(p) = path {
            paths.insert(p);
        }
    }
    paths
}

fn porcelain_v2_path(line: &str) -> Option<String> {
    let parts: Vec<_> = line.split_whitespace().collect();
    match parts.first().copied()? {
        "1" | "2" => parts.get(8).map(|s| (*s).to_owned()),
        "u" => parts.get(1).map(|s| (*s).to_owned()),
        "?" => parts.get(1).map(|s| (*s).to_owned()),
        "!" => parts.get(1).map(|s| (*s).to_owned()),
        _ => None,
    }
}

fn grit_change_paths(repo: &Path, args: &[&str]) -> BTreeSet<String> {
    let v = grit_json(repo, args);
    let mut out = BTreeSet::new();
    for key in ["staged", "unstaged"] {
        if let Some(arr) = v[key].as_array() {
            for item in arr {
                out.insert(item["path"].as_str().unwrap().to_owned());
                if let Some(old) = item.get("old_path").and_then(|x| x.as_str()) {
                    out.insert(old.to_owned());
                }
            }
        }
    }
    if let Some(arr) = v["untracked"].as_array() {
        for item in arr {
            out.insert(item.as_str().unwrap().trim_end_matches('/').to_owned());
        }
    }
    if let Some(arr) = v["conflicts"].as_array() {
        for item in arr {
            out.insert(item.as_str().unwrap().to_owned());
        }
    }
    out
}

fn init_repo(repo: &Path) -> TestResult {
    grit_cmd().args(["init", "."]).current_dir(repo).status()?;
    git_ok(repo, &["config", "user.email", "t@example.com"]);
    git_ok(repo, &["config", "user.name", "Test"]);
    Ok(())
}

#[test]
fn status_path_limited_json_matches_git_porcelain_v2() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::create_dir_all(repo.join("keep"))?;
    fs::create_dir_all(repo.join("skip"))?;
    fs::write(repo.join("keep/a.txt"), b"a\n")?;
    fs::write(repo.join("skip/b.txt"), b"b\n")?;
    init_repo(repo)?;
    git_ok(repo, &["add", "."]);
    git_ok(repo, &["commit", "-qm", "seed"]);
    fs::write(repo.join("keep/a.txt"), b"a2\n")?;
    fs::write(repo.join("keep/new.txt"), b"n\n")?;
    grit_cmd()
        .args(["add", "keep/new.txt"])
        .current_dir(repo)
        .status()?;
    fs::write(repo.join("keep/untracked.txt"), b"u\n")?;
    fs::write(repo.join("skip/outside.txt"), b"x\n")?;

    let spec = ["keep"];
    let grit_set = grit_change_paths(repo, &["status", "--", "keep"]);
    let git_set = git_porcelain_v2_paths(repo, &spec);
    assert_eq!(grit_set, git_set);
    assert!(!grit_set.iter().any(|p| p.starts_with("skip/")));
    Ok(())
}

#[test]
fn status_staged_git_mv_json_reports_old_path() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("a.txt"), b"x\n")?;
    init_repo(repo)?;
    git_ok(repo, &["add", "a.txt"]);
    git_ok(repo, &["commit", "-qm", "seed"]);
    git_ok(repo, &["mv", "a.txt", "b.txt"]);

    let v = grit_json(repo, &["status"]);
    let staged = v["staged"].as_array().unwrap();
    let renamed: Vec<_> = staged.iter().filter(|e| e["status"] == "renamed").collect();
    assert_eq!(renamed.len(), 1);
    assert_eq!(renamed[0]["path"], "b.txt");
    assert_eq!(renamed[0]["old_path"], "a.txt");
    Ok(())
}

#[test]
fn status_untracked_no_omits_untracked() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("lonely.txt"), b"x\n")?;
    init_repo(repo)?;

    let v = grit_json(repo, &["status", "--untracked=no"]);
    assert_eq!(v["untracked"].as_array().unwrap().len(), 0);
    Ok(())
}

#[test]
fn status_modified_json_omits_old_path() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("old.txt"), b"v1\n")?;
    init_repo(repo)?;
    git_ok(repo, &["add", "old.txt"]);
    git_ok(repo, &["commit", "-qm", "seed"]);
    fs::write(repo.join("old.txt"), b"v2\n")?;

    let v = grit_json(repo, &["status"]);
    let unstaged = v["unstaged"].as_array().unwrap();
    let modified = unstaged
        .iter()
        .find(|e| e["status"] == "modified")
        .expect("modified entry");
    assert_eq!(modified["path"], "old.txt");
    assert!(modified.get("old_path").is_none());
    Ok(())
}

#[test]
fn status_path_named_uno_after_double_dash_matches_git() -> TestResult {
    let base = tempfile::tempdir()?;
    let repo = base.path();
    fs::write(repo.join("-uno"), b"v1\n")?;
    init_repo(repo)?;
    git_ok(repo, &["add", "--", "-uno"]);
    git_ok(repo, &["commit", "-qm", "seed"]);
    fs::write(repo.join("-uno"), b"v2\n")?;

    let grit = grit_json(repo, &["status", "--", "-uno"]);
    assert_eq!(grit["clean"], false);
    assert_eq!(grit["unstaged"].as_array().unwrap().len(), 1);
    assert_eq!(grit["unstaged"][0]["path"], "-uno");
    assert_eq!(grit["unstaged"][0]["status"], "modified");

    let git = git_cmd()
        .args(["status", "--porcelain=v2", "--", "-uno"])
        .current_dir(repo)
        .output()
        .expect("git status");
    assert!(git.status.success());
    let line = String::from_utf8_lossy(&git.stdout);
    assert!(
        line.contains("-uno") && line.contains('M'),
        "git must report modified -uno: {line}"
    );
    Ok(())
}
