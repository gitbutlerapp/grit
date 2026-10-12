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

fn snapshot_fresh(name: &str, args: &[&str], prepare: impl Fn(&CliFixture)) {
    CliFixture::snapshot_fresh_all_modes(name, args, snapshot_dir(), || {
        let fixture = CliFixture::new();
        prepare(&fixture);
        fixture
    });
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

#[test]
fn snapshot_commit() {
    snapshot_fresh("commit", &["commit", "-m", "third"], |_| {});
}

#[test]
fn snapshot_commit_amend() {
    CliFixture::snapshot_fresh_all_modes(
        "commit_amend",
        &["commit", "--amend", "-m", "third (amended)"],
        snapshot_dir(),
        || {
            let fixture = CliFixture::new();
            fixture
                .run(&["commit", "-m", "third"])
                .expect_success("commit third");
            fixture
        },
    );
}

#[test]
fn snapshot_add() {
    snapshot_fresh("add", &["add", "tracked.txt"], |_| {});
}

#[test]
fn snapshot_switch() {
    snapshot_fresh("switch", &["switch", "feature"], |fixture| {
        fixture
            .run(&["commit", "-m", "third"])
            .expect_success("commit third");
    });
}

#[test]
fn snapshot_switch_create() {
    snapshot_fresh("switch_create", &["switch", "--create", "topic"], |_| {});
}

#[test]
fn snapshot_merge_ff() {
    snapshot_fresh("merge_ff", &["merge", "main"], |fixture| {
        fixture
            .run(&["commit", "-m", "third"])
            .expect_success("commit third");
        fixture
            .run(&["switch", "feature"])
            .expect_success("switch feature");
    });
}

#[test]
fn snapshot_merge_conflict() {
    CliFixture::snapshot_fresh_all_modes(
        "merge_conflict",
        &["merge", "side"],
        snapshot_dir(),
        || {
            let fixture = CliFixture::empty();
            prepare_merge_conflict(&fixture);
            fixture
        },
    );
}

#[test]
fn snapshot_pick() {
    snapshot_fresh("pick", &["pick", "topic"], prepare_pick);
}

#[test]
fn snapshot_revert() {
    snapshot_fresh("revert", &["revert", "HEAD"], |fixture| {
        fixture
            .run(&["commit", "-m", "third"])
            .expect_success("commit third");
    });
}

fn prepare_merge_conflict(fixture: &CliFixture) {
    write_file(fixture.path(), "f.txt", "base\n");
    fixture.run(&["add", "f.txt"]).expect_success("add");
    fixture
        .run(&["commit", "-m", "base"])
        .expect_success("commit");
    fixture
        .run(&["switch", "--create", "side"])
        .expect_success("branch side");
    write_file(fixture.path(), "f.txt", "side\n");
    fixture
        .run(&["commit", "-m", "side"])
        .expect_success("commit side");
    fixture
        .run(&["switch", "main"])
        .expect_success("switch main");
    write_file(fixture.path(), "f.txt", "main\n");
    fixture
        .run(&["commit", "-m", "main"])
        .expect_success("commit main");
}

fn prepare_pick(fixture: &CliFixture) {
    fixture
        .run(&["commit", "-m", "third"])
        .expect_success("commit third");
    fixture
        .run(&["switch", "--create", "topic"])
        .expect_success("create topic");
    write_file(fixture.path(), "picked.txt", "picked\n");
    fixture.run(&["add", "picked.txt"]).expect_success("add");
    fixture
        .run(&["commit", "-m", "pick me"])
        .expect_success("commit");
    fixture
        .run(&["switch", "main"])
        .expect_success("switch main");
}

fn write_file(dir: &Path, name: &str, contents: &str) {
    std::fs::write(dir.join(name), contents).unwrap_or_else(|e| panic!("write {name}: {e}"));
}

trait ExpectSuccess {
    fn expect_success(self, label: &str) -> grit_test_support::Output;
}

impl ExpectSuccess for grit_test_support::Output {
    fn expect_success(self, label: &str) -> grit_test_support::Output {
        assert!(
            self.ok(),
            "grit {label} failed: exit={:?}\nstdout:\n{}\nstderr:\n{}",
            self.status,
            self.stdout,
            self.stderr
        );
        self
    }
}
