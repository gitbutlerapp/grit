//! Golden snapshots of human, `--json`, and `--markdown` CLI output.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::path::Path;

use grit_test_support::CliFixture;

fn snapshot_dir() -> &'static Path {
    Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots"))
}

fn snapshot_all_modes(name: &str, args: &[&str]) {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes(name, args, snapshot_dir());
}

#[test]
fn snapshot_status() {
    snapshot_all_modes("status", &["status"]);
}

#[test]
fn snapshot_log() {
    snapshot_all_modes("log", &["log"]);
}

#[test]
fn snapshot_log_json_filter_commits_oid() {
    snapshot_all_modes(
        "log_json_filter_commits_oid",
        &["log", "--filter", ".commits[].oid"],
    );
}

#[test]
fn snapshot_diff() {
    snapshot_all_modes("diff", &["diff"]);
}

#[test]
fn snapshot_diff_staged() {
    snapshot_all_modes("diff_staged", &["diff", "--staged"]);
}

#[test]
fn snapshot_show_head() {
    snapshot_all_modes("show_head", &["show", "HEAD"]);
}

#[test]
fn snapshot_show_unknown_revision_errors() {
    snapshot_all_modes("show_unknown_revision", &["show", "not-a-ref"]);
}
