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
    fixture.write_repo_file(".mailmap", "Mapped <mapped@example.com> <raw@example.com>\n");
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
