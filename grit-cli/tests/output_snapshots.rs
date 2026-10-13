//! Golden snapshots of human, `--json`, and `--markdown` CLI output.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_test_support::{CliFixture, SnapshotExitCodes};

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
fn snapshot_skill() {
    snapshot_all_modes("skill", &["skill"]);
}

#[test]
fn snapshot_log() {
    snapshot_all_modes("log", &["log"]);
}

#[test]
fn snapshot_log_json_filter_commits_oid() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes_exits(
        "log_json_filter_commits_oid",
        &["log", "--filter", ".commits[].oid"],
        snapshot_dir(),
        SnapshotExitCodes {
            human: 1,
            json: 0,
            markdown: 1,
        },
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
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes_expect(
        "show_unknown_revision",
        &["show", "not-a-ref"],
        snapshot_dir(),
        1,
    );
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

fn snapshot_workspace(name: &str, args: &[&str]) {
    let fixture = CliFixture::workspace();
    fixture.snapshot_all_modes(name, args, snapshot_dir());
}

#[test]
fn snapshot_shortlog() {
    snapshot_all_modes("shortlog", &["shortlog"]);
}

#[test]
fn snapshot_blame_tracked() {
    snapshot_all_modes("blame_tracked", &["blame", "tracked.txt"]);
}

#[test]
fn snapshot_blame_tracked_line_range() {
    snapshot_all_modes(
        "blame_tracked_line_range",
        &["blame", "tracked.txt", "-L", "1,2"],
    );
}

#[test]
fn snapshot_branch_list() {
    snapshot_all_modes("branch_list", &["branch"]);
}

#[test]
fn snapshot_branch_create() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes("branch_create", &["branch", "wip"], snapshot_dir());
}

#[test]
fn snapshot_branch_delete() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["branch", "delete-me"]);
    fixture.snapshot_all_modes(
        "branch_delete",
        &["branch", "-d", "delete-me"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_tag_list() {
    snapshot_all_modes("tag_list", &["tag"]);
}

#[test]
fn snapshot_tag_lightweight() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes("tag_lightweight", &["tag", "v1.0"], snapshot_dir());
}

#[test]
fn snapshot_tag_annotated() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes(
        "tag_annotated",
        &["tag", "-m", "release note", "v2.0"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_tag_delete() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["tag", "drop-me"]);
    fixture.snapshot_all_modes("tag_delete", &["tag", "-d", "drop-me"], snapshot_dir());
}

#[test]
fn snapshot_config_get_unset() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes_expect(
        "config_get_unset",
        &["config", "user.email"],
        snapshot_dir(),
        1,
    );
}

#[test]
fn snapshot_config_set() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes(
        "config_set",
        &["config", "user.email", "snap@example.com"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_config_list() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["config", "user.email", "snap@example.com"]);
    fixture.snapshot_all_modes("config_list", &["config", "--list"], snapshot_dir());
}

#[test]
fn snapshot_config_unset() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["config", "user.email", "snap@example.com"]);
    fixture.snapshot_all_modes(
        "config_unset",
        &["config", "--unset", "user.email"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_remote_list() {
    snapshot_all_modes("remote_list", &["remote"]);
}

#[test]
fn snapshot_remote_add() {
    let fixture = CliFixture::new();
    let bare = fixture.path().join("upstream.git");
    fixture.run_ok(&["init", "--bare", bare.to_str().expect("utf8 bare path")]);
    fixture.snapshot_all_modes(
        "remote_add",
        &[
            "remote",
            "add",
            "upstream",
            bare.to_str().expect("utf8 bare path"),
        ],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_bundle_create() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes(
        "bundle_create",
        &["bundle", "create", "snap.bundle", "HEAD"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_bundle_verify() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["bundle", "create", "snap.bundle", "HEAD"]);
    fixture.snapshot_all_modes(
        "bundle_verify",
        &["bundle", "verify", "snap.bundle"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_bundle_verify_invalid() {
    let fixture = CliFixture::new();
    fs::write(fixture.path().join("invalid.bundle"), b"not a git bundle\n")
        .expect("write invalid bundle");
    fixture.snapshot_all_modes_expect(
        "bundle_verify_invalid",
        &["bundle", "verify", "invalid.bundle"],
        snapshot_dir(),
        1,
    );
}

#[test]
fn snapshot_bundle_list() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["bundle", "create", "snap.bundle", "HEAD"]);
    fixture.snapshot_all_modes(
        "bundle_list",
        &["bundle", "list", "snap.bundle"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_init() {
    snapshot_workspace("init", &["init"]);
}

#[test]
fn snapshot_init_bare() {
    snapshot_workspace("init_bare", &["init", "--bare"]);
}

#[test]
fn snapshot_rm() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes("rm", &["rm", "second.txt"], snapshot_dir());
}

#[test]
fn snapshot_rm_cached() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes(
        "rm_cached",
        &["rm", "--cached", "second.txt"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_mv() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes("mv", &["mv", "second.txt", "renamed.txt"], snapshot_dir());
}

#[test]
fn snapshot_clean_dry_run() {
    snapshot_all_modes("clean_dry_run", &["clean"]);
}

#[test]
fn snapshot_clean_force() {
    snapshot_all_modes("clean_force", &["clean", "--force"]);
}

#[test]
fn snapshot_reset_soft() {
    snapshot_all_modes("reset_soft", &["reset", "--soft", "HEAD~1"]);
}

#[test]
fn snapshot_reset_mixed() {
    snapshot_all_modes("reset_mixed", &["reset", "--mixed", "HEAD~1"]);
}

#[test]
fn snapshot_reset_hard() {
    snapshot_all_modes("reset_hard", &["reset", "--hard", "HEAD~1"]);
}

#[test]
fn snapshot_restore_worktree() {
    snapshot_all_modes("restore_worktree", &["restore", "tracked.txt"]);
}

#[test]
fn snapshot_restore_staged() {
    snapshot_all_modes("restore_staged", &["restore", "--staged", "staged.txt"]);
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

fn prepare_stash(fixture: &CliFixture) {
    fixture
        .run(&["stash", "push", "-m", "snapshot stash"])
        .expect_success("stash push");
}

const LINKED_WT: &str = "linked-wt";

#[test]
fn snapshot_stash_push() {
    snapshot_fresh("stash_push", &["stash", "push", "-m", "wip"], |_| {});
}

#[test]
fn snapshot_stash_list() {
    snapshot_fresh("stash_list", &["stash", "list"], prepare_stash);
}

#[test]
fn snapshot_stash_show() {
    snapshot_fresh("stash_show", &["stash", "show"], prepare_stash);
}

#[test]
fn snapshot_stash_apply() {
    snapshot_fresh("stash_apply", &["stash", "apply"], prepare_stash);
}

#[test]
fn snapshot_stash_pop() {
    snapshot_fresh("stash_pop", &["stash", "pop"], prepare_stash);
}

#[test]
fn snapshot_stash_drop() {
    snapshot_fresh("stash_drop", &["stash", "drop"], prepare_stash);
}

#[test]
fn snapshot_worktree_list() {
    snapshot_fresh("worktree_list", &["worktree", "list"], |fixture| {
        fixture
            .run(&["worktree", "add", LINKED_WT, "-b", "wt|branch"])
            .expect_success("worktree add");
    });
}

#[test]
fn snapshot_worktree_add() {
    snapshot_fresh(
        "worktree_add",
        &["worktree", "add", LINKED_WT, "-b", "wt-new"],
        |_| {},
    );
}

#[test]
fn snapshot_worktree_remove() {
    snapshot_fresh(
        "worktree_remove",
        &["worktree", "remove", LINKED_WT, "-f"],
        |fixture| {
            fixture
                .run(&["worktree", "add", LINKED_WT, "-b", "wt-rm"])
                .expect_success("worktree add");
        },
    );
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
