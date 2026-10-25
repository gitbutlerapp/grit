//! `grit merge` on a file both branches renamed to different names (issue #1878).

use serde_json::Value;
use std::error::Error;
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

type TestResult = Result<(), Box<dyn Error>>;

fn null_config() -> &'static str {
    if cfg!(windows) {
        "NUL"
    } else {
        "/dev/null"
    }
}

fn git(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn grit(dir: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_grit"))
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", null_config())
        .env("GIT_CONFIG_SYSTEM", null_config())
        .output()
        .expect("grit")
}

/// The issue's reproduction: `a` renamed to `b` on `main` and to `c` on `topic`.
fn setup(root: &Path) -> TestResult {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["config", "user.name", "T"]);
    git(root, &["config", "user.email", "t@e.com"]);
    fs::write(root.join("a"), "line1\nline2\nline3\nline4\n")?;
    git(root, &["add", "a"]);
    git(root, &["commit", "-qm", "base"]);
    git(root, &["switch", "-qc", "topic"]);
    git(root, &["mv", "a", "c"]);
    git(root, &["commit", "-qm", "a->c"]);
    git(root, &["switch", "-q", "main"]);
    git(root, &["mv", "a", "b"]);
    git(root, &["commit", "-qm", "a->b"]);
    Ok(())
}

fn conflict_rows(json: &Value) -> Vec<(String, String)> {
    json["conflicts"]
        .as_array()
        .expect("conflicts array")
        .iter()
        .map(|c| {
            (
                c["path"].as_str().unwrap_or_default().to_owned(),
                c["kind"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect()
}

fn expected_rows() -> Vec<(String, String)> {
    ["a", "b", "c"]
        .iter()
        .map(|p| ((*p).to_owned(), "rename_rename".to_owned()))
        .collect()
}

#[test]
fn merge_check_reports_rename_rename_conflict() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    setup(root)?;
    let head = git(root, &["rev-parse", "HEAD"]);

    let out = grit(root, &["merge", "--check", "--json", "topic"]);
    assert_eq!(
        out.status.code(),
        Some(1),
        "--check must answer 'conflicts'"
    );
    let json: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert_eq!(json["check"], true);
    assert_eq!(conflict_rows(&json), expected_rows());
    assert_eq!(git(root, &["rev-parse", "HEAD"]), head);
    assert!(git(root, &["status", "--porcelain"]).is_empty());
    Ok(())
}

#[test]
fn merge_stops_with_git_index_layout() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    setup(root)?;
    let head = git(root, &["rev-parse", "HEAD"]);

    let out = grit(root, &["merge", "--json", "topic"]);
    assert_eq!(out.status.code(), Some(1));
    let json: Value = serde_json::from_slice(&out.stdout)?;
    assert_eq!(json["result"], "conflicts");
    assert_eq!(conflict_rows(&json), expected_rows());
    let summary = json["conflicts"][0]["summary"].as_str().unwrap_or_default();
    assert!(
        summary.contains("a renamed to b") && summary.contains("to c in topic"),
        "{summary}"
    );
    assert!(json["conflicts"][1].get("summary").is_none());
    assert!(json["conflicts"][2].get("summary").is_none());
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]),
        head,
        "branch must not move"
    );

    let stages: Vec<String> = git(root, &["ls-files", "-s"])
        .lines()
        .map(|l| {
            let mut parts = l.split_whitespace();
            let _mode = parts.next();
            let _oid = parts.next();
            format!(
                "{} {}",
                parts.next().unwrap_or(""),
                parts.next().unwrap_or("")
            )
        })
        .collect();
    assert_eq!(stages, vec!["1 a", "2 b", "3 c"]);
    assert!(root.join("b").is_file() && root.join("c").is_file());
    assert!(!root.join("a").exists());
    Ok(())
}

#[test]
fn merge_human_output_names_both_destinations_once() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    setup(root)?;

    let out = grit(root, &["merge", "topic"]);
    assert_eq!(out.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&out.stderr);
    let lines: Vec<&str> = stderr
        .lines()
        .filter(|l| l.starts_with("CONFLICT"))
        .collect();
    assert_eq!(
        lines,
        vec![
            "CONFLICT (rename_rename): a renamed to b on the current branch and to c in topic; both names are in the work tree",
            "CONFLICT (rename_rename): b",
            "CONFLICT (rename_rename): c",
        ],
        "{stderr}"
    );
    Ok(())
}

#[test]
fn removing_the_old_and_one_new_name_lets_continue_commit() -> TestResult {
    let tmp = tempfile::tempdir()?;
    let root = tmp.path();
    setup(root)?;
    assert_eq!(grit(root, &["merge", "topic"]).status.code(), Some(1));

    let rm = grit(root, &["rm", "a", "c"]);
    assert!(
        rm.status.success(),
        "{}",
        String::from_utf8_lossy(&rm.stderr)
    );
    let cont = grit(root, &["merge", "--continue"]);
    assert!(
        cont.status.success(),
        "{}",
        String::from_utf8_lossy(&cont.stderr)
    );
    let tree = git(root, &["ls-tree", "-r", "--name-only", "HEAD"]);
    assert_eq!(tree.lines().collect::<Vec<_>>(), vec!["b"]);
    assert_eq!(
        git(root, &["log", "-1", "--format=%P"])
            .split_whitespace()
            .count(),
        2,
        "--continue must record a two-parent merge"
    );
    git(root, &["fsck", "--strict"]);
    Ok(())
}
