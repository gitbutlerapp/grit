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
fn snapshot_fsck() {
    snapshot_all_modes("fsck", &["fsck"]);
}

#[test]
fn snapshot_fsck_missing_blob() {
    snapshot_fresh("fsck_missing_blob", &["fsck"], |fixture| {
        use grit_lib::index::Index;
        use grit_lib::porcelain::add::stage_for_commit;
        use grit_lib::progress::NullProgress;
        use grit_lib::repo::{InitOptions, Repository};
        use std::fs;

        let repo = Repository::init(fixture.path(), &InitOptions::default()).expect("init");
        fs::write(repo.work_tree().unwrap().join("orphan.txt"), b"data\n").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let index = Index::load(&repo.git_dir().join("index")).expect("index");
        let oid = index.entries()[0].oid;
        let loose = repo
            .odb()
            .enumerate_local_loose_objects()
            .expect("loose")
            .into_iter()
            .find(|(o, _)| *o == oid)
            .expect("path")
            .1;
        fs::remove_file(loose).expect("remove");
    });
}

#[test]
fn snapshot_gc_dry_run() {
    snapshot_all_modes("gc", &["gc", "--dry-run"]);
}

#[test]
fn snapshot_gc_repack() {
    snapshot_fresh("gc_repack", &["gc", "repack"], |fixture| {
        fixture.write_repo_file("repack-extra.txt", "extra payload\n");
        fixture.run_ok(&["add", "repack-extra.txt"]);
        fixture.run_ok(&["commit", "-m", "repack extra"]);
    });
}

#[test]
fn snapshot_gc_commit_graph() {
    snapshot_all_modes("gc_commit_graph", &["gc", "commit-graph"]);
}

#[test]
fn snapshot_gc_pack_refs() {
    snapshot_all_modes("gc_pack_refs", &["gc", "pack-refs"]);
}

#[test]
fn snapshot_gc_prune() {
    snapshot_all_modes("gc_prune", &["gc", "prune"]);
}

#[test]
fn snapshot_gc_midx() {
    snapshot_fresh("gc_midx", &["gc", "midx"], |fixture| {
        fixture.run_ok(&["gc", "repack"]);
    });
}

#[test]
fn snapshot_repack_dry_run() {
    snapshot_all_modes("repack", &["repack", "--dry-run"]);
}

#[test]
fn snapshot_log() {
    snapshot_all_modes("log", &["log"]);
}

#[test]
fn snapshot_reflog() {
    snapshot_all_modes("reflog", &["reflog"]);
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
fn snapshot_history_reword() {
    snapshot_fresh(
        "history_reword",
        &["history", "reword", "HEAD~1", "-m", "reworded subject"],
        |_| {},
    );
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
fn snapshot_revs_head() {
    snapshot_all_modes("revs_head", &["revs", "HEAD"]);
}

#[test]
fn snapshot_revs_count() {
    snapshot_all_modes("revs_count", &["revs", "--count", "HEAD"]);
}

#[test]
fn snapshot_merge_base() {
    snapshot_all_modes("merge_base", &["merge-base", "HEAD~1", "HEAD"]);
}

#[test]
fn snapshot_merge_base_is_ancestor() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes_exits(
        "merge_base_is_ancestor",
        &["merge-base", "--is-ancestor", "HEAD~1", "HEAD"],
        snapshot_dir(),
        grit_test_support::SnapshotExitCodes {
            human: 0,
            json: 0,
            markdown: 0,
        },
    );
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
fn snapshot_notes_list() {
    snapshot_all_modes("notes_list", &["notes"]);
}

#[test]
fn snapshot_notes_show() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["notes", "add", "HEAD", "-m", "snapshot note"]);
    fixture.snapshot_all_modes("notes_show", &["notes", "show", "HEAD"], snapshot_dir());
}

#[test]
fn snapshot_notes_add() {
    let fixture = CliFixture::new();
    fixture.snapshot_all_modes(
        "notes_add",
        &["notes", "add", "HEAD", "-m", "hello notes"],
        snapshot_dir(),
    );
}

#[test]
fn snapshot_notes_remove() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["notes", "add", "HEAD", "-m", "to remove"]);
    fixture.snapshot_all_modes("notes_remove", &["notes", "remove", "HEAD"], snapshot_dir());
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
fn snapshot_bundle_unbundle() {
    let fixture = CliFixture::new();
    fixture.run_ok(&["bundle", "create", "snap.bundle", "HEAD"]);
    fixture.snapshot_all_modes(
        "bundle_unbundle",
        &["bundle", "unbundle", "snap.bundle"],
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

#[test]
fn snapshot_resolve_head() {
    snapshot_all_modes("resolve_head", &["resolve", "HEAD", "HEAD^{tree}"]);
}

#[test]
fn snapshot_resolve_short() {
    snapshot_all_modes("resolve_short", &["resolve", "--short", "HEAD"]);
}

#[test]
fn snapshot_cat_blob() {
    snapshot_all_modes("cat_blob", &["cat", "HEAD:tracked.txt"]);
}

#[test]
fn snapshot_cat_commit() {
    snapshot_all_modes("cat_commit", &["cat", "HEAD^{commit}"]);
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

fn prepare_ignore_check(fixture: &CliFixture) {
    write_file(fixture.path(), ".gitignore", "*.log\n");
    write_file(fixture.path(), "noise.log", "noise\n");
    write_file(fixture.path(), "keep.txt", "ok\n");
}

#[test]
fn snapshot_ignore_check() {
    snapshot_fresh(
        "ignore_check",
        &["ignore", "check", "noise.log"],
        prepare_ignore_check,
    );
}

#[test]
fn snapshot_ignore_check_all() {
    snapshot_fresh(
        "ignore_check_all",
        &["ignore", "check", "--all", "noise.log", "keep.txt"],
        prepare_ignore_check,
    );
}

#[test]
fn snapshot_ignore_check_json() {
    let fixture = CliFixture::new();
    prepare_ignore_check(&fixture);
    fixture.snapshot_all_modes_exits(
        "ignore_check_json",
        &["ignore", "check", "keep.txt"],
        snapshot_dir(),
        SnapshotExitCodes::all(1),
    );
}

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

#[test]
fn snapshot_worktree_lock() {
    snapshot_fresh(
        "worktree_lock",
        &["worktree", "lock", LINKED_WT, "--reason", "hold"],
        |fixture| {
            fixture
                .run(&["worktree", "add", LINKED_WT, "-b", "wt-lock"])
                .expect_success("worktree add");
        },
    );
}

#[test]
fn snapshot_worktree_unlock() {
    snapshot_fresh(
        "worktree_unlock",
        &["worktree", "unlock", LINKED_WT],
        |fixture| {
            fixture
                .run(&["worktree", "add", LINKED_WT, "-b", "wt-unlock"])
                .expect_success("worktree add");
            fixture
                .run(&["worktree", "lock", LINKED_WT, "--reason", "hold"])
                .expect_success("worktree lock");
        },
    );
}

#[test]
fn snapshot_hooks_list() {
    snapshot_fresh("hooks_list", &["hooks", "list"], |fixture| {
        let hook = fixture.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o100755))
                .expect("chmod");
        }
    });
}

#[test]
fn snapshot_hooks_run() {
    snapshot_fresh("hooks_run", &["hooks", "run", "pre-commit"], |fixture| {
        let hook = fixture.path().join(".git/hooks/pre-commit");
        std::fs::write(&hook, "#!/bin/sh\nexit 0\n").expect("write hook");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o100755))
                .expect("chmod");
        }
    });
}

#[test]
fn snapshot_hooks_run_propagates_exit_code() {
    let snapshot_dir = snapshot_dir();
    let fixture = CliFixture::new();
    let hook = fixture.path().join(".git/hooks/pre-commit");
    std::fs::write(&hook, "#!/bin/sh\nexit 17\n").expect("write hook");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o100755)).expect("chmod");
    }
    fixture.snapshot_all_modes_expect(
        "hooks_run_exit_17",
        &["hooks", "run", "pre-commit"],
        &snapshot_dir,
        17,
    );
}

#[test]
fn snapshot_patch_id() {
    snapshot_all_modes("patch_id", &["patch-id", "HEAD"]);
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
