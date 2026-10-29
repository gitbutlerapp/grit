//! `grit status`, `grit log` and `grit commit` on a GitButler workspace: `HEAD` on
//! `gitbutler/workspace`, topped by GitButler's merge of the applied branches.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;
use std::process::{Command, Output};

use serde_json::Value;

const GRIT: &str = env!("CARGO_BIN_EXE_grit");

fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .expect("run git");
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .expect("utf8")
        .trim()
        .to_owned()
}

fn grit(root: &Path, args: &[&str]) -> Output {
    Command::new(GRIT)
        .current_dir(root)
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "A")
        .env("GIT_AUTHOR_EMAIL", "a@example.com")
        .env("GIT_COMMITTER_NAME", "A")
        .env("GIT_COMMITTER_EMAIL", "a@example.com")
        .output()
        .expect("run grit")
}

fn grit_json(root: &Path, args: &[&str]) -> Value {
    let out = grit(root, args);
    assert!(
        out.status.success(),
        "grit {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).expect("json")
}

/// `main`, branches `one` (two commits) and `two` (one commit), and a GitButler workspace
/// merging them, checked out. Returns the workspace commit.
fn workspace_repo(root: &Path, message: &str) -> String {
    git(root, &["init", "-q", "-b", "main"]);
    git(root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    for (name, commits) in [("one", 2), ("two", 1)] {
        git(root, &["switch", "-q", "-c", name, "main"]);
        for i in 0..commits {
            std::fs::write(root.join(format!("{name}.txt")), format!("{i}")).unwrap();
            git(root, &["add", "."]);
            git(root, &["commit", "-q", "-m", &format!("{name} {i}")]);
        }
    }
    git(root, &["switch", "-q", "-c", "gitbutler/workspace", "one"]);
    git(root, &["merge", "-q", "--no-ff", "two", "-m", message]);
    git(root, &["rev-parse", "HEAD"])
}

#[test]
fn status_lists_the_applied_branches_instead_of_the_workspace_commit() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let workspace = workspace_repo(root, "GitButler Workspace Commit\n\nmanaged by GitButler");
    let v = grit_json(root, &["status", "--json"]);
    let ws = &v["gitbutler_workspace"];
    assert_eq!(ws["commit"], workspace.as_str());
    let heads = ws["heads"].as_array().expect("heads");
    assert_eq!(heads.len(), 2, "{v}");
    assert_eq!(heads[0]["branches"], serde_json::json!(["one"]));
    assert_eq!(
        heads[0]["commit"],
        git(root, &["rev-parse", "one"]).as_str()
    );
    assert_eq!(heads[0]["subject"], "one 1");
    assert_eq!(heads[1]["branches"], serde_json::json!(["two"]));
    assert!(
        v["commits"]
            .as_array()
            .expect("commits")
            .iter()
            .all(|c| c["oid"] != workspace.as_str()),
        "the workspace commit is left out of the commits ahead: {v}"
    );

    let out = grit(root, &["status"]);
    let human = String::from_utf8_lossy(&out.stdout);
    assert!(
        human.starts_with("On GitButler workspace  ·  2 branches applied"),
        "{human}"
    );
    assert!(
        human.contains("one  ") && human.contains("one 1"),
        "{human}"
    );
    assert!(
        !human.contains("GitButler Workspace Commit"),
        "workspace commit hidden: {human}"
    );
}

#[test]
fn status_on_an_ordinary_branch_has_no_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    workspace_repo(root, "an ordinary merge");
    let v = grit_json(root, &["status", "--json"]);
    assert!(v.get("gitbutler_workspace").is_none(), "{v}");
}

#[test]
fn log_starts_from_the_applied_branches() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let workspace = workspace_repo(root, "GitButler Workspace Commit");
    let v = grit_json(root, &["log", "--json"]);
    let subjects: Vec<&str> = v["commits"]
        .as_array()
        .expect("commits")
        .iter()
        .map(|c| c["subject"].as_str().expect("subject"))
        .collect();
    assert_eq!(
        subjects.len(),
        4,
        "both branches and the base: {subjects:?}"
    );
    for subject in ["one 0", "one 1", "two 0", "base"] {
        assert!(subjects.contains(&subject), "{subject} in {subjects:?}");
    }
    assert!(!subjects.contains(&"GitButler Workspace Commit"));

    // Naming a revision walks exactly that, workspace commit included.
    let v = grit_json(root, &["log", "--json", "HEAD", "-n", "1"]);
    assert_eq!(v["commits"][0]["oid"], workspace.as_str());
}

#[test]
fn commit_is_refused_on_a_workspace() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();
    let workspace = workspace_repo(root, "GitButler Workspace Commit");
    std::fs::write(root.join("new.txt"), "new").unwrap();
    let out = grit(root, &["commit", "--json", "add new"]);
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).expect("json error");
    assert_eq!(v["kind"], "gitbutler_workspace", "{v}");
    assert_eq!(
        git(root, &["rev-parse", "HEAD"]),
        workspace,
        "HEAD did not move"
    );

    git(root, &["switch", "-q", "-c", "work"]);
    let out = grit(root, &["commit", "add new"]);
    assert!(
        out.status.success(),
        "an ordinary branch commits: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}
