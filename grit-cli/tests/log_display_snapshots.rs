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
            f.write_repo_file("f", "a\n");
            f.run_ok(&["commit", "-m", "root"]);
            f.run_ok(&["branch", "topic"]);
            f.write_repo_file("t", "t\n");
            f.run_ok(&["commit", "-m", "topic"]);
            f.run_ok(&["checkout", "main"]);
            f.write_repo_file("m", "m\n");
            f.run_ok(&["commit", "-m", "mainline"]);
            git(f.path(), &["merge", "--no-ff", "-m", "merge", "topic"]);
        },
    );
}

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
