//! Snapshots for `grit log --oneline`, `--decorate`, and `--group-by author`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_test_support::{git, CliFixture};

fn snapshot_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

fn snapshot_fresh(name: &str, args: &[&str], prepare: impl Fn(&CliFixture)) {
    CliFixture::snapshot_fresh_all_modes(name, args, snapshot_dir(), || {
        let fixture = CliFixture::new();
        prepare(&fixture);
        fixture
    });
}

#[test]
fn log_oneline_snapshot() {
    snapshot_fresh("log_oneline", &["log", "--oneline", "--limit", "3"], |f| {
        f.run_ok(&["init"]);
        f.write_repo_file("a.txt", "one\n");
        f.run_ok(&["commit", "-m", "first"]);
        f.write_repo_file("a.txt", "two\n");
        f.run_ok(&["commit", "-m", "second"]);
        f.write_repo_file("a.txt", "three\n");
        f.run_ok(&["commit", "-m", "third"]);
    });
}

#[test]
fn log_decorate_snapshot() {
    snapshot_fresh(
        "log_decorate",
        &["log", "--decorate", "--oneline", "--limit", "1"],
        |f| {
            f.run_ok(&["init"]);
            f.write_repo_file("a.txt", "x\n");
            f.run_ok(&["commit", "-m", "only"]);
            f.run_ok(&["tag", "v1"]);
        },
    );
}

#[test]
fn log_graph_rejects_json() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["init"]);
    fixture.write_repo_file("a.txt", "x\n");
    fixture.run_ok(&["commit", "-m", "only"]);
    let out = fixture.run(&["log", "--graph", "--json"]);
    assert!(!out.ok());
    assert_eq!(out.status, Some(1));
    assert!(out.stdout.contains("incompatible_flags"));
    assert!(out.stdout.contains("--graph"));
}

#[test]
fn log_graph_linear() {
    snapshot_fresh(
        "log_graph_linear",
        &["log", "--graph", "--oneline", "-n", "3"],
        |f| {
            f.run_ok(&["init"]);
            f.write_repo_file("a.txt", "one\n");
            f.run_ok(&["commit", "-m", "first"]);
            f.write_repo_file("a.txt", "two\n");
            f.run_ok(&["commit", "-m", "second"]);
            f.write_repo_file("a.txt", "three\n");
            f.run_ok(&["commit", "-m", "third"]);
        },
    );
}

#[test]
fn log_graph_merge() {
    snapshot_fresh(
        "log_graph_merge",
        &["log", "--graph", "--oneline", "-n", "10"],
        |f| {
            f.run_ok(&["init"]);
            let root = f.path();
            git(root, &["checkout", "-B", "main"]);
            std::fs::write(root.join("f"), "a\n").expect("write");
            git(root, &["add", "f"]);
            git(root, &["commit", "-m", "root"]);
            git(root, &["checkout", "-b", "topic"]);
            std::fs::write(root.join("t"), "t\n").expect("write");
            git(root, &["add", "t"]);
            git(root, &["commit", "-m", "topic"]);
            git(root, &["checkout", "main"]);
            std::fs::write(root.join("m"), "m\n").expect("write");
            git(root, &["add", "m"]);
            git(root, &["commit", "-m", "mainline"]);
            git(root, &["merge", "--no-ff", "-m", "merge", "topic"]);
        },
    );
}

#[test]
fn log_group_by_author_count_json() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["init"]);
    fixture.write_repo_file("a.txt", "a\n");
    fixture.run_ok(&["commit", "-m", "one"]);
    fixture.write_repo_file("a.txt", "b\n");
    fixture.run_ok(&["commit", "-m", "two"]);
    let raw = fixture
        .run_ok(&["log", "--group-by", "author", "--count", "--json"])
        .stdout;
    let out: serde_json::Value = serde_json::from_str(&raw).expect("json");
    let groups = out["groups"].as_array().expect("groups array");
    assert!(!groups.is_empty());
    assert!(groups[0]["commits"].is_null());
    assert!(groups[0]["count"].as_u64().unwrap() >= 1);
}
