//! Tests for [`grit_lib::revwalk::history::undo_last_rewrite`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;
use std::process::Command;

use grit_lib::refs::{resolve_ref, write_ref};
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_rewrite, drop as history_drop, rebase_onto, undo_last_rewrite, ApplyOptions, EmptyPolicy,
    HistoryError, RebaseOntoOptions, RefScope, RewriteOptions,
};

use support::{
    build_commit_dag, commit_file, git, git_fsck_strict, git_rev_parse, open_grit_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    open_grit_repo(wt)
}

fn rewrite_opts() -> RewriteOptions {
    RewriteOptions {
        scope: RefScope::Branches,
        empty: EmptyPolicy::Drop,
        committer: "Undo Tester <undo@grit.test> 1 +0000"
            .parse()
            .expect("signature"),
        preserve_committer: false,
        sign: false,
    }
}

fn apply_opts(operation: &str) -> ApplyOptions {
    ApplyOptions {
        identity: "Undo Tester <undo@grit.test>".into(),
        reflog_message: format!("history {operation}"),
        record_operation: Some(operation.into()),
    }
}

fn undo_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "Undo Tester <undo@grit.test>".into(),
        reflog_message: "history undo".into(),
        record_operation: None,
    }
}

fn drop_and_apply(grit: &Repository, commit: grit_lib::objects::ObjectId) {
    let plan = history_drop(grit, commit, &rewrite_opts()).expect("drop plan");
    apply_rewrite(grit, &plan, &apply_opts("drop")).expect("apply drop");
}

#[test]
fn undo_restores_refs_after_drop() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let before = git_rev_parse(wt, "HEAD").expect("tip");
    drop_and_apply(&grit, oids["b"]);
    let after_drop = git_rev_parse(wt, "HEAD").expect("after drop");
    assert_ne!(before, after_drop);

    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(before));
    git_fsck_strict(wt);
}

#[test]
fn undo_after_rebase_restores_stacked_branches() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(
        wt,
        &[("u", "r"), ("a", "u"), ("b", "a"), ("c", "b"), ("d", "b")],
        &[("c", "refs/heads/main"), ("d", "refs/heads/stacked")],
    );
    let onto = commit_file(wt, "base.txt", "base\n", "onto");
    let grit = open_grit(wt);
    let main_before = resolve_ref(grit.git_dir(), "refs/heads/main").expect("main");
    let stacked_before = resolve_ref(grit.git_dir(), "refs/heads/stacked").expect("stacked");
    let plan = rebase_onto(
        &grit,
        oids["u"],
        onto,
        &[oids["c"]],
        &RebaseOntoOptions {
            rewrite: rewrite_opts(),
            skip_applied: true,
            flatten_leaf_merges: false,
            favor: grit_lib::diff::merge_file::MergeFavor::default(),
        },
    )
    .expect("rebase plan");
    apply_rewrite(&grit, &plan, &apply_opts("rebase")).expect("apply rebase");
    assert_ne!(
        resolve_ref(grit.git_dir(), "refs/heads/main").expect("main"),
        main_before
    );

    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/main").expect("main"),
        main_before
    );
    assert_eq!(
        resolve_ref(grit.git_dir(), "refs/heads/stacked").expect("stacked"),
        stacked_before
    );
    git_fsck_strict(wt);
}

#[test]
fn undo_twice_redoes() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let original = git_rev_parse(wt, "HEAD").expect("tip");
    drop_and_apply(&grit, oids["b"]);
    let dropped = git_rev_parse(wt, "HEAD").expect("after drop");
    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(original));
    undo_last_rewrite(&grit, &undo_opts(), false).expect("redo");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(dropped));
}

#[test]
fn undo_refuses_when_ref_moved() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    drop_and_apply(&grit, oids["b"]);
    write_ref(grit.git_dir(), "refs/heads/main", &oids["a"]).expect("move ref");
    let err = undo_last_rewrite(&grit, &undo_opts(), false).expect_err("stale");
    assert!(matches!(err, HistoryError::UndoStale { .. }));
}

#[test]
fn undo_refuses_on_dirty_overlap() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "track.t", "1\n", "a");
    commit_file(wt, "track.t", "2\n", "b");
    let tip = git_rev_parse(wt, "HEAD").expect("b");
    commit_file(wt, "track.t", "3\n", "c");
    let grit = open_grit(wt);
    drop_and_apply(&grit, git_rev_parse(wt, "HEAD").expect("c"));
    fs::write(wt.join("track.t"), "dirty\n").expect("dirty");
    let _ = tip;
    let err = undo_last_rewrite(&grit, &undo_opts(), false).expect_err("dirty");
    assert!(matches!(
        err,
        HistoryError::LocalChangesWouldBeOverwritten { .. }
    ));
}

#[test]
fn undo_without_record_errors() {
    let repo = HistoryRepo::init();
    let grit = open_grit(repo.worktree());
    let err = undo_last_rewrite(&grit, &undo_opts(), false).expect_err("no record");
    assert!(matches!(err, HistoryError::NoUndoRecord));
}

#[test]
fn undo_record_ignored_by_git_fsck() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    drop_and_apply(&grit, oids["b"]);
    assert!(
        grit.git_dir().join("grit/history-last").is_file(),
        "record written"
    );
    git_fsck_strict(wt);
}

#[test]
fn undo_survives_git_gc_with_reflog_updates_disabled() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "core.logAllRefUpdates", "false"]);
    commit_file(wt, "track.t", "1\n", "a");
    commit_file(wt, "track.t", "2\n", "b");
    commit_file(wt, "track.t", "3\n", "c");
    let dropped_tip = git_rev_parse(wt, "HEAD").expect("tip");
    let grit = open_grit(wt);
    drop_and_apply(&grit, dropped_tip);
    assert_ne!(git_rev_parse(wt, "HEAD"), Some(dropped_tip));

    let gc = Command::new("git")
        .current_dir(wt)
        .args(["gc", "--prune=now"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("git gc");
    assert!(gc.success(), "git gc --prune=now");

    let cat = Command::new("git")
        .current_dir(wt)
        .args([
            "cat-file",
            "-e",
            &format!("{}^{{commit}}", dropped_tip.to_hex()),
        ])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .status()
        .expect("cat-file");
    assert!(
        cat.success(),
        "dropped tip must stay reachable via reflog after gc"
    );

    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo after gc");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(dropped_tip));
}

#[test]
fn undo_refuses_when_recorded_ref_deleted() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    drop_and_apply(&grit, oids["b"]);
    git(wt, &["update-ref", "-d", "refs/heads/main"]);
    let err = undo_last_rewrite(&grit, &undo_opts(), false).expect_err("stale");
    assert!(matches!(err, HistoryError::UndoStale { .. }));
}

#[test]
fn undo_old_commits_remain_in_reflog() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    let dropped_tip = oids["c"];
    drop_and_apply(&grit, oids["b"]);
    undo_last_rewrite(&grit, &undo_opts(), false).expect("undo");
    let log = Command::new("git")
        .current_dir(wt)
        .args(["reflog", "show", "main"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("reflog");
    let text = String::from_utf8_lossy(&log.stdout);
    assert!(text.contains(&dropped_tip.to_hex()[..7]));
}
