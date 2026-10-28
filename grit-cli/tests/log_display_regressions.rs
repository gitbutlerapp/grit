//! Regressions for log decoration, mailmap config, and JSON empty log.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use grit_test_support::{git, CliFixture};

mod support {
    use std::path::Path;
    use std::process::Command;

    pub fn git_commit_as(root: &Path, name: &str, email: &str, msg: &str) {
        std::fs::write(root.join("f.txt"), format!("{msg}\n")).expect("write");
        let status = Command::new("git")
            .current_dir(root)
            .args(["add", "f.txt"])
            .status()
            .expect("git add");
        assert!(status.success());
        let status = Command::new("git")
            .current_dir(root)
            .env("GIT_AUTHOR_NAME", name)
            .env("GIT_AUTHOR_EMAIL", email)
            .env("GIT_COMMITTER_NAME", name)
            .env("GIT_COMMITTER_EMAIL", email)
            .env("GIT_AUTHOR_DATE", "1700000000 +0000")
            .env("GIT_COMMITTER_DATE", "1700000000 +0000")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .args(["commit", "-m", msg])
            .status()
            .expect("git commit");
        assert!(status.success());
    }
}

#[test]
fn log_markdown_decorate_default_layout_includes_refs() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["init"]);
    fixture.write_repo_file("a.txt", "x\n");
    fixture.run_ok(&["commit", "-m", "only"]);
    fixture.run_ok(&["tag", "v1"]);
    let out = fixture
        .run_ok(&["--markdown", "log", "--decorate", "--limit", "1"])
        .stdout;
    assert!(
        out.contains("tag: v1") || out.contains("HEAD"),
        "expected ref decorations in markdown output: {out}"
    );
}

#[test]
fn log_decorate_folds_head_onto_current_branch_not_arbitrary_same_tip() {
    let fixture = CliFixture::empty();
    let root = fixture.path();
    fixture.write_repo_file("a.txt", "x\n");
    fixture.run_ok(&["commit", "-m", "tip"]);
    git(root, &["branch", "zzz"]);
    let out = fixture
        .run_ok(&["log", "--decorate", "--oneline", "--limit", "1"])
        .stdout;
    assert!(
        out.contains("HEAD -> main"),
        "expected HEAD folded onto current branch main, got: {out}"
    );
    assert!(
        !out.contains("HEAD -> zzz"),
        "should not fold HEAD onto zzz when on main: {out}"
    );
}

#[test]
fn log_group_by_respects_log_mailmap_false() {
    let fixture = CliFixture::empty();
    let root = fixture.path();
    fixture.run_ok(&["config", "log.mailmap", "false"]);
    fixture.write_repo_file(
        ".mailmap",
        "Mapped <mapped@example.com> <raw@example.com>\n",
    );
    support::git_commit_as(root, "Raw", "raw@example.com", "one");
    let out = fixture
        .run_ok(&["log", "--group-by", "author", "--count"])
        .stdout;
    assert!(
        out.contains("raw@example.com"),
        "log.mailmap=false should use raw author: {out}"
    );
    assert!(
        !out.contains("mapped@example.com"),
        "mailmap must not apply when log.mailmap=false: {out}"
    );
}

#[test]
fn log_group_by_author_count_respects_max_count() {
    let fixture = CliFixture::empty();
    let root = fixture.path();
    support::git_commit_as(root, "Alice", "alice@example.com", "first");
    support::git_commit_as(root, "Bob", "bob@example.com", "second");
    support::git_commit_as(root, "Carol", "carol@example.com", "third");
    let raw = fixture
        .run_ok(&[
            "--json",
            "log",
            "--group-by",
            "author",
            "--count",
            "-n",
            "1",
        ])
        .stdout;
    let out: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let groups = out["groups"].as_array().expect("groups");
    assert_eq!(groups.len(), 1, "expected one author group for -n 1: {out}");
    assert_eq!(groups[0]["name"], "Carol");
    assert_eq!(out["truncated"], true);
}

#[test]
fn log_oneline_human_lists_commits_from_oneline_lines() {
    let fixture = CliFixture::empty();
    for i in 0..5 {
        fixture.write_repo_file("a.txt", &format!("v{i}\n"));
        fixture.run_ok(&["commit", "-m", &format!("commit {i}")]);
    }
    let out = fixture.run_ok(&["log", "--oneline", "-n", "5"]).stdout;
    assert!(
        !out.contains("No commits yet."),
        "plain oneline must not treat empty commits vec as empty history: {out}"
    );
    assert_eq!(
        out.lines().filter(|l| !l.trim().is_empty()).count(),
        5,
        "{out}"
    );
}

#[test]
fn log_oneline_markdown_lists_commits_from_oneline_lines() {
    let fixture = CliFixture::empty();
    for i in 0..5 {
        fixture.write_repo_file("a.txt", &format!("v{i}\n"));
        fixture.run_ok(&["commit", "-m", &format!("commit {i}")]);
    }
    let out = fixture
        .run_ok(&["--markdown", "log", "--oneline", "-n", "5"])
        .stdout;
    assert!(
        !out.contains("No commits yet."),
        "markdown oneline must not treat empty commit_rows as empty history: {out}"
    );
    assert_eq!(
        out.lines().filter(|l| l.starts_with("- `")).count(),
        5,
        "{out}"
    );
}

#[test]
fn log_empty_json_includes_commits_array() {
    let fixture = CliFixture::empty();
    fixture.write_repo_file("a.txt", "x\n");
    fixture.run_ok(&["commit", "-m", "only"]);
    let raw = fixture.run_ok(&["log", "--json", "--limit", "0"]).stdout;
    let out: serde_json::Value = serde_json::from_str(&raw).expect("json");
    assert!(
        out.get("commits").is_some(),
        "commits key must be present: {out}"
    );
}

#[test]
fn log_graph_unicode_implies_graph_and_uses_unicode_lanes() {
    let fixture = CliFixture::empty();
    fixture.write_repo_file("a.txt", "one\n");
    fixture.run_ok(&["commit", "-m", "one"]);
    fixture.write_repo_file("a.txt", "two\n");
    fixture.run_ok(&["commit", "-m", "two"]);
    let out = fixture
        .run_ok(&["log", "--graph-unicode", "-n", "5"])
        .stdout;
    assert!(
        out.contains('●'),
        "expected Unicode graph lanes with --graph-unicode alone: {out}"
    );
}

#[test]
fn log_graph_ascii_uses_star_lanes() {
    let fixture = CliFixture::empty();
    fixture.write_repo_file("a.txt", "one\n");
    fixture.run_ok(&["commit", "-m", "one"]);
    fixture.write_repo_file("a.txt", "two\n");
    fixture.run_ok(&["commit", "-m", "two"]);
    let out = fixture
        .run_ok(&["log", "--graph", "--graph-ascii", "-n", "5"])
        .stdout;
    assert!(out.contains('*'), "expected ASCII graph lanes: {out}");
    assert!(
        !out.contains('●'),
        "ASCII graph must not use Unicode bullets: {out}"
    );
}
