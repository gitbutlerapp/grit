//! Tests for [`grit_lib::revwalk::history::undo_last_rewrite`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::process::Command;

use grit_lib::diff::{hunk_ids, DiffSide};
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::hunk_select::HunkSelection;
use grit_lib::progress::CancelToken;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    AbsorbOptions, ApplyOptions, EmptyPolicy, FixupOptions, HistoryError, RebaseOntoOptions,
    RewriteOptions,
};

use support::{
    build_commit_dag, commit_file, git, git_fsck_strict, git_rev_parse, open_grit_repo, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    open_grit_repo(wt)
}

fn rewrite_opts() -> RewriteOptions {
    RewriteOptions::new(
        "Undo Tester <undo@grit.test> 1 +0000"
            .parse()
            .expect("signature"),
    )
    .with_sign(false)
}

fn apply_opts(operation: &str) -> ApplyOptions {
    ApplyOptions::new(
        support::reflog_signature("Undo Tester <undo@grit.test>"),
        format!("history {operation}"),
    )
    .with_record_operation(Some(operation.into()))
}

fn undo_opts() -> ApplyOptions {
    ApplyOptions::new(
        support::reflog_signature("Undo Tester <undo@grit.test>"),
        "history undo",
    )
}

fn drop_and_apply(grit: &Repository, commit: grit_lib::objects::ObjectId) {
    let plan = grit
        .history()
        .drop_commit(commit, &rewrite_opts())
        .expect("drop plan");
    grit.history()
        .apply(
            &plan,
            &apply_opts("drop"),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply drop");
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

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
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
    let main_before = grit.references().resolve("refs/heads/main").expect("main");
    let stacked_before = grit
        .references()
        .resolve("refs/heads/stacked")
        .expect("stacked");
    let plan = grit
        .history()
        .rebase_onto(
            oids["u"],
            onto,
            &[oids["c"]],
            &RebaseOntoOptions::new(rewrite_opts()),
        )
        .expect("rebase plan");
    grit.history()
        .apply(
            &plan,
            &apply_opts("rebase"),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply rebase");
    assert_ne!(
        grit.references().resolve("refs/heads/main").expect("main"),
        main_before
    );

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(
        grit.references().resolve("refs/heads/main").expect("main"),
        main_before
    );
    assert_eq!(
        grit.references()
            .resolve("refs/heads/stacked")
            .expect("stacked"),
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
    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(original));
    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("redo");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(dropped));
}

#[test]
fn undo_refuses_when_ref_moved() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let oids = build_commit_dag(wt, &[("b", "a"), ("c", "b")], &[("c", "refs/heads/main")]);
    let grit = open_grit(wt);
    drop_and_apply(&grit, oids["b"]);
    grit_test_support::refs::set_ref(&grit, "refs/heads/main", &oids["a"]).expect("move ref");
    let err = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect_err("stale");
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
    let err = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect_err("dirty");
    assert!(matches!(
        err,
        HistoryError::LocalChangesWouldBeOverwritten { .. }
    ));
}

#[test]
fn undo_without_record_errors() {
    let repo = HistoryRepo::init();
    let grit = open_grit(repo.worktree());
    let err = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect_err("no record");
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
    // `git gc` expires reflog entries for unreachable commits older than
    // `gc.reflogExpireUnreachable` (30 days), so the forced reflog entry that keeps the
    // dropped tip alive must carry a current timestamp, not the fixed test epoch.
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    let drop_opts = (apply_opts("drop")).with_identity(
        format!("Undo Tester <undo@grit.test> {now} +0000")
            .parse()
            .expect("signature"),
    );
    let plan = grit
        .history()
        .drop_commit(dropped_tip, &rewrite_opts())
        .expect("drop plan");
    grit.history()
        .apply(
            &plan,
            &drop_opts,
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("apply drop");
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

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo after gc");
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
    let err = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect_err("stale");
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
    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
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

fn fixup_opts(selection: Option<HunkSelection>) -> FixupOptions {
    FixupOptions {
        message: None,
        empty: EmptyPolicy::Drop,
        rewrite: rewrite_opts(),
        selection,
        pathspecs: Vec::new(),
    }
}

fn fixup_staged_into(grit: &Repository, target: ObjectId) {
    let staged = grit.history().staged_paths().expect("staged paths");
    let plan = grit
        .history()
        .fixup(target, &fixup_opts(None))
        .expect("fixup plan");
    grit.history()
        .apply_fixup(&plan, &apply_opts("fixup"), &staged, &CancelToken::never())
        .expect("apply fixup");
}

fn status_porcelain(wt: &std::path::Path) -> String {
    git(wt, &["status", "--porcelain", "--untracked-files=no"])
}

fn read(wt: &std::path::Path, path: &str) -> String {
    fs::read_to_string(wt.join(path)).expect("read work tree file")
}

/// Three commits `c1` (a.txt), `c2` (b.txt), `c3` (c.txt), as in the issue report.
fn three_commit_repo() -> (HistoryRepo, ObjectId, ObjectId) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let c1 = commit_file(wt, "a.txt", "a\n", "c1");
    commit_file(wt, "b.txt", "b\n", "c2");
    let c3 = commit_file(wt, "c.txt", "c\n", "c3");
    (repo, c1, c3)
}

#[test]
fn undo_after_staged_fixup_restores_the_staged_edit() {
    let (repo, c1, c3) = three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("a.txt"), "a\na-fixed\n").expect("edit");
    git(wt, &["add", "a.txt"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    let fixed_tip = git_rev_parse(wt, "HEAD").expect("fixed tip");
    assert_eq!(status_porcelain(wt), "");

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(outcome.restored_paths, vec!["a.txt".to_owned()]);
    assert_eq!(outcome.unrestored_edits, None);
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(c3));
    assert_eq!(status_porcelain(wt), "M  a.txt\n");
    assert_eq!(git(wt, &["show", ":a.txt"]), "a\na-fixed\n");
    assert_eq!(read(wt, "a.txt"), "a\na-fixed\n");
    git_fsck_strict(wt);

    let redo = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("redo");
    assert_eq!(redo.restored_paths, vec!["a.txt".to_owned()]);
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(fixed_tip));
    assert_eq!(status_porcelain(wt), "");
    assert_eq!(read(wt, "a.txt"), "a\na-fixed\n");

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo again");
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(c3));
    assert_eq!(status_porcelain(wt), "M  a.txt\n");
    git_fsck_strict(wt);
}

#[test]
fn undo_after_fixup_restores_staged_and_unstaged_layers() {
    let (repo, c1, _) = three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("a.txt"), "a\na-fixed\n").expect("stage edit");
    git(wt, &["add", "a.txt"]);
    fs::write(wt.join("a.txt"), "a\na-fixed\nunstaged\n").expect("unstaged edit");
    fs::write(wt.join("new.txt"), "new\n").expect("new file");
    git(wt, &["add", "new.txt"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    assert_eq!(status_porcelain(wt), " M a.txt\n");

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(
        outcome.restored_paths,
        vec!["a.txt".to_owned(), "new.txt".to_owned()]
    );
    assert_eq!(status_porcelain(wt), "MM a.txt\nA  new.txt\n");
    assert_eq!(git(wt, &["show", ":a.txt"]), "a\na-fixed\n");
    assert_eq!(read(wt, "a.txt"), "a\na-fixed\nunstaged\n");
    assert_eq!(read(wt, "new.txt"), "new\n");
    git_fsck_strict(wt);

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("redo");
    assert_eq!(status_porcelain(wt), " M a.txt\n");
    assert_eq!(read(wt, "a.txt"), "a\na-fixed\nunstaged\n");
}

#[test]
fn undo_dry_run_reports_restored_paths_without_touching_anything() {
    let (repo, c1, _) = three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("a.txt"), "a\na-fixed\n").expect("edit");
    git(wt, &["add", "a.txt"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    let fixed_tip = git_rev_parse(wt, "HEAD");

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::DryRun,
            &CancelToken::never(),
        )
        .expect("dry run");
    assert!(outcome.dry_run);
    assert_eq!(outcome.restored_paths, vec!["a.txt".to_owned()]);
    assert_eq!(git_rev_parse(wt, "HEAD"), fixed_tip);
    assert_eq!(status_porcelain(wt), "");
}

#[test]
fn undo_after_fixup_refuses_when_a_restored_path_was_edited() {
    let (repo, c1, _) = three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("a.txt"), "a\na-fixed\n").expect("edit");
    git(wt, &["add", "a.txt"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    let fixed_tip = git_rev_parse(wt, "HEAD");
    fs::write(wt.join("a.txt"), "a\na-fixed\nlater\n").expect("later edit");

    let err = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect_err("edited");
    assert!(
        matches!(&err, HistoryError::LocalChangesWouldBeOverwritten { paths } if paths == &["a.txt"]),
        "{err:?}"
    );
    assert_eq!(git_rev_parse(wt, "HEAD"), fixed_tip);
    assert_eq!(read(wt, "a.txt"), "a\na-fixed\nlater\n");
}

#[test]
fn undo_after_absorb_restores_assigned_hunks_and_keeps_unassigned_staged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base = commit_file(wt, "root.txt", "r\n", "root");
    commit_file(wt, "a.txt", "a\n", "a");
    let tip = commit_file(wt, "tail.txt", "t\n", "tail");
    fs::write(wt.join("a.txt"), "a\nfix-a\n").expect("assigned edit");
    fs::write(wt.join("root.txt"), "r2\n").expect("edit of a line older than base");
    git(wt, &["add", "a.txt", "root.txt"]);
    let grit = open_grit(wt);
    let plan = grit
        .history()
        .absorb(&AbsorbOptions {
            base: Some(base),
            rewrite: rewrite_opts(),
        })
        .expect("absorb plan");
    assert_eq!(plan.assignments.len(), 1);
    assert_eq!(plan.unassigned.len(), 1);
    grit.history()
        .apply_absorb(&plan, &apply_opts("absorb"), &CancelToken::never())
        .expect("apply absorb");
    let absorbed_tip = git_rev_parse(wt, "HEAD");
    assert_eq!(git(wt, &["diff", "--cached", "--name-only"]), "root.txt\n");

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(
        outcome.restored_paths,
        vec!["a.txt".to_owned(), "root.txt".to_owned()]
    );
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(tip));
    assert_eq!(status_porcelain(wt), "M  a.txt\nM  root.txt\n");
    assert_eq!(git(wt, &["show", ":a.txt"]), "a\nfix-a\n");
    assert_eq!(read(wt, "root.txt"), "r2\n");
    git_fsck_strict(wt);

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("redo");
    assert_eq!(git_rev_parse(wt, "HEAD"), absorbed_tip);
    assert_eq!(git(wt, &["diff", "--cached", "--name-only"]), "root.txt\n");
}

#[test]
fn undo_after_hunk_selection_fixup_restores_the_work_tree() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base: String = (0..24).map(|i| format!("line{i}\n")).collect();
    let edited = base
        .replacen("line5\n", "LINE5\n", 1)
        .replacen("line18\n", "LINE18\n", 1);
    let target = commit_file(wt, "f.txt", &base, "init");
    let tip = commit_file(wt, "other.t", "x\n", "second");
    fs::write(wt.join("f.txt"), &edited).expect("edit");
    let grit = open_grit(wt);
    let files = grit.index().unstaged_file_diffs(&[]).expect("diff");
    let (file, peer) = &files[0];
    let ids = hunk_ids(DiffSide::Unstaged, &file.path, &file.hunks, peer);
    let selection = HunkSelection {
        ids: vec![ids[0].clone()],
        ranges: Vec::new(),
    };
    let plan = grit
        .history()
        .fixup(target, &fixup_opts(Some(selection.clone())))
        .expect("plan");
    grit.history()
        .apply_fixup_hunks(
            &plan,
            &apply_opts("fixup"),
            &selection,
            &[],
            &CancelToken::never(),
        )
        .expect("apply hunk fixup");
    assert_eq!(read(wt, "f.txt"), edited);

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(outcome.restored_paths, vec!["f.txt".to_owned()]);
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(tip));
    assert_eq!(status_porcelain(wt), " M f.txt\n");
    assert_eq!(read(wt, "f.txt"), edited);
    git_fsck_strict(wt);
}

#[test]
fn undo_of_a_fixup_record_without_saved_state_names_the_commit_holding_the_edit() {
    let (repo, c1, c3) = three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("a.txt"), "a\na-fixed\n").expect("edit");
    git(wt, &["add", "a.txt"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    let fixed_tip = git_rev_parse(wt, "HEAD").expect("fixed tip");
    fs::remove_file(grit.git_dir().join("grit/history-last-work")).expect("drop sidecar");

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert!(outcome.restored_paths.is_empty());
    assert_eq!(outcome.unrestored_edits, Some(fixed_tip));
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(c3));
    assert_eq!(
        git(wt, &["show", &format!("{}~2:a.txt", fixed_tip.to_hex())]),
        "a\na-fixed\n"
    );
}

#[test]
fn a_later_rewrite_discards_the_saved_fixup_state() {
    let (repo, c1, _) = three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("a.txt"), "a\na-fixed\n").expect("edit");
    git(wt, &["add", "a.txt"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    let sidecar = grit.git_dir().join("grit/history-last-work");
    assert!(sidecar.is_file());
    git_fsck_strict(wt);

    let tip = git_rev_parse(wt, "HEAD").expect("tip");
    drop_and_apply(&grit, tip);
    assert!(
        !sidecar.exists(),
        "drop record must not inherit fixup state"
    );
    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo drop");
    assert!(outcome.restored_paths.is_empty());
    assert_eq!(outcome.unrestored_edits, None);
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(tip));
    assert_eq!(status_porcelain(wt), "");
}

#[test]
fn undo_after_fixup_restores_a_staged_submodule_pointer() {
    let upstream = HistoryRepo::init();
    let up = upstream.worktree();
    let s1 = commit_file(up, "s.txt", "1\n", "s1");
    let s2 = commit_file(up, "s.txt", "2\n", "s2");

    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "a.txt", "a\n", "c1");
    let url = up.to_str().expect("utf-8 path");
    git(
        wt,
        &[
            "-c",
            "protocol.file.allow=always",
            "submodule",
            "add",
            "-q",
            url,
            "sub",
        ],
    );
    git(&wt.join("sub"), &["checkout", "-q", &s1.to_hex()]);
    git(wt, &["add", "sub"]);
    git(wt, &["commit", "-q", "-m", "add sub"]);
    let target = git_rev_parse(wt, "HEAD").expect("target");
    let tip = commit_file(wt, "b.txt", "b\n", "c3");
    git(&wt.join("sub"), &["checkout", "-q", &s2.to_hex()]);
    git(wt, &["add", "sub"]);
    let grit = open_grit(wt);
    fixup_staged_into(&grit, target);
    let fixed_tip = git_rev_parse(wt, "HEAD").expect("fixed tip");
    assert_eq!(git_rev_parse(wt, "HEAD:sub"), Some(s2));
    assert_eq!(status_porcelain(wt), "");

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(outcome.restored_paths, vec!["sub".to_owned()]);
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(tip));
    assert_eq!(git_rev_parse(wt, "HEAD:sub"), Some(s1));
    assert_eq!(git_rev_parse(wt, ":sub"), Some(s2));
    assert_eq!(status_porcelain(wt), "M  sub\n");
    assert_eq!(git_rev_parse(&wt.join("sub"), "HEAD"), Some(s2));
    git_fsck_strict(wt);

    let redo = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("redo");
    assert_eq!(redo.restored_paths, vec!["sub".to_owned()]);
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(fixed_tip));
    assert_eq!(git_rev_parse(wt, ":sub"), Some(s2));
    assert_eq!(status_porcelain(wt), "");
    assert_eq!(git_rev_parse(&wt.join("sub"), "HEAD"), Some(s2));
    git_fsck_strict(wt);
}

/// Three CRLF commits under `core.autocrlf=true`, as in the issue report.
fn crlf_three_commit_repo() -> (HistoryRepo, ObjectId) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    git(wt, &["config", "core.autocrlf", "true"]);
    let c1 = commit_file(wt, "f1.txt", "c1\r\n", "c1");
    commit_file(wt, "f2.txt", "c2\r\n", "c2");
    commit_file(wt, "f3.txt", "c3\r\n", "c3");
    (repo, c1)
}

/// `git ls-files --debug` size of `path`'s index entry.
fn index_entry_size(wt: &std::path::Path, path: &str) -> u64 {
    let debug = git(wt, &["ls-files", "--debug", "--", path]);
    let size = debug
        .split_whitespace()
        .skip_while(|word| *word != "size:")
        .nth(1)
        .expect("size field");
    size.parse().expect("numeric size")
}

fn grit_status_paths(grit: &Repository) -> (Vec<String>, Vec<String>) {
    let model = grit
        .status()
        .run(
            &grit_lib::porcelain::status::StatusOptions::default(),
            &mut grit_lib::progress::NullProgress,
            &CancelToken::never(),
        )
        .expect("grit status");
    let paths = |entries: &[grit_lib::diff::DiffEntry]| {
        entries.iter().map(|e| e.path().to_owned()).collect()
    };
    (paths(&model.staged), paths(&model.unstaged))
}

#[test]
fn undo_after_fixup_restores_a_staged_crlf_file_stat_clean() {
    let (repo, c1) = crlf_three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("f1.txt"), "edit\r\n").expect("edit");
    git(wt, &["add", "f1.txt"]);
    let staged_blob = git_rev_parse(wt, ":f1.txt").expect("staged blob");
    assert_eq!(status_porcelain(wt), "M  f1.txt\n");
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);

    let outcome = grit
        .history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(outcome.restored_paths, vec!["f1.txt".to_owned()]);
    assert_eq!(status_porcelain(wt), "M  f1.txt\n");
    assert_eq!(git_rev_parse(wt, ":f1.txt"), Some(staged_blob));
    assert_eq!(fs::read(wt.join("f1.txt")).expect("read"), b"edit\r\n");
    assert_eq!(git(wt, &["diff"]), "");
    assert!(git(wt, &["diff", "--cached"]).contains("+edit"));
    assert_eq!(index_entry_size(wt, "f1.txt"), 6);
    git(wt, &["update-index", "--refresh"]);
    let grit = open_grit(wt);
    assert_eq!(
        grit_status_paths(&grit),
        (vec!["f1.txt".to_owned()], Vec::new())
    );
    git_fsck_strict(wt);

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("redo");
    assert_eq!(status_porcelain(wt), "");
    git(wt, &["update-index", "--refresh"]);
}

#[test]
fn undo_after_fixup_keeps_an_unstaged_crlf_edit_visible() {
    let (repo, c1) = crlf_three_commit_repo();
    let wt = repo.worktree();
    fs::write(wt.join("f1.txt"), "edit\r\n").expect("edit");
    git(wt, &["add", "f1.txt"]);
    let staged_blob = git_rev_parse(wt, ":f1.txt").expect("staged blob");
    fs::write(wt.join("f1.txt"), "edit\r\nmore\r\n").expect("unstaged edit");
    let grit = open_grit(wt);
    fixup_staged_into(&grit, c1);
    assert_eq!(status_porcelain(wt), " M f1.txt\n");

    grit.history()
        .undo_last(
            &undo_opts(),
            grit_lib::revwalk::history::UndoMode::Apply,
            &CancelToken::never(),
        )
        .expect("undo");
    assert_eq!(status_porcelain(wt), "MM f1.txt\n");
    assert_eq!(git_rev_parse(wt, ":f1.txt"), Some(staged_blob));
    assert_eq!(
        fs::read(wt.join("f1.txt")).expect("read"),
        b"edit\r\nmore\r\n"
    );
    assert!(git(wt, &["diff"]).contains("+more"));
    assert!(git(wt, &["diff", "--cached"]).contains("+edit"));
    let grit = open_grit(wt);
    assert_eq!(
        grit_status_paths(&grit),
        (vec!["f1.txt".to_owned()], vec!["f1.txt".to_owned()])
    );
    git_fsck_strict(wt);
}
