//! Tests for [`grit_lib::revwalk::history::fixup`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use crate::support;

use std::fs;
use std::process::Command;
use std::str::FromStr;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::hunk_id::{hunk_ids_with_duplicate_peers, DiffSide, HunkId};
use grit_lib::diff::DiffStatus;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::hunk_select::{
    unstaged_file_diffs, HunkSelectError, HunkSelection, LineRange,
};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NeverCancel;
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_fixup_hunk_selection, apply_fixup_rewrite, fixup, staged_fixup_paths, ApplyOptions,
    EmptyPolicy, FixupOptions, HistoryError, RefScope, RewriteOptions,
};

use support::{
    assert_fixup_tip_matches_git_oracle, assert_fixup_tip_structure_matches, commit_file,
    disable_grit_change_ids_for_git_oracle, git, git_fsck_strict, git_history_fixup_legacy_oracle,
    git_history_fixup_or_legacy, git_history_subcommand_available, git_rev_parse,
    harness_committer_line, reset_harness_commit_clock, HistoryRepo,
};

fn open_grit(wt: &std::path::Path) -> Repository {
    Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover")
}

fn committer(seq: i64) -> String {
    format!("Fixup <f@grit.test> {seq} +0000")
}

fn fixup_opts(scope: RefScope, empty: EmptyPolicy) -> FixupOptions {
    FixupOptions {
        message: None,
        empty,
        rewrite: RewriteOptions {
            scope,
            empty,
            committer: committer(1).parse().expect("signature"),
            preserve_committer: false,
            sign: true,
        },
        selection: None,
        pathspecs: Vec::new(),
    }
}

fn harness_fixup_opts(scope: RefScope, empty: EmptyPolicy) -> FixupOptions {
    FixupOptions {
        message: None,
        empty,
        rewrite: RewriteOptions {
            scope,
            empty,
            committer: harness_committer_line().parse().expect("signature"),
            preserve_committer: false,
            sign: true,
        },
        selection: None,
        pathspecs: Vec::new(),
    }
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: support::reflog_signature("Fixup <f@grit.test>"),
        reflog_message: "history fixup".into(),
        record_operation: None,
        hooks: grit_lib::PostOperationHooks::Run,
    }
}

fn plan_and_apply(grit: &Repository, wt: &std::path::Path, target: ObjectId, opts: &FixupOptions) {
    let staged = staged_fixup_paths(grit).expect("staged paths");
    let plan = fixup(grit, target, opts).expect("plan");
    apply_fixup_rewrite(grit, &plan, &apply_opts(), &staged).expect("apply");
    git_fsck_strict(wt);
}

fn linear_repo_with_file() -> (HistoryRepo, ObjectId) {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "initial.t", "i\n", "initial");
    commit_file(wt, "file.txt", "content\n", "add file");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "third.t", "t\n", "third");
    (repo, target)
}

#[test]
fn fixup_applies_staged_changes_to_target() {
    let (repo, target) = linear_repo_with_file();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "content\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        target,
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    let parent = git_rev_parse(wt, "HEAD~1").expect("parent");
    let fixed = parse_commit(&grit.odb().read(&parent).expect("read").data).expect("parse");
    let blob = grit.odb().read(&fixed.tree).expect("tree");
    let entries = grit_lib::objects::parse_tree(&blob.data).expect("parse tree");
    let file = entries
        .iter()
        .find(|e| e.name == b"file.txt")
        .expect("file");
    let content = grit.odb().read(&file.oid).expect("blob");
    assert_eq!(String::from_utf8_lossy(&content.data), "content\nfix\n");
}

#[test]
fn fixup_preserves_message_and_author() {
    let (repo, target) = linear_repo_with_file();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "content\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    let plan = fixup(
        &grit,
        target,
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    )
    .expect("plan");
    let new_oid = plan
        .commits
        .iter()
        .find(|r| r.old == target)
        .and_then(|r| r.new)
        .expect("new");
    let old = parse_commit(&grit.odb().read(&target).expect("read").data).expect("old");
    let new = parse_commit(&grit.odb().read(&new_oid).expect("read").data).expect("new");
    assert_eq!(old.message, new.message);
    assert_eq!(old.author, new.author);
}

#[test]
fn fixup_replaces_message_when_given() {
    let (repo, target) = linear_repo_with_file();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "content\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let grit = open_grit(wt);
    let mut opts = fixup_opts(RefScope::Head, EmptyPolicy::Drop);
    opts.message = Some("replacement subject\n".into());
    let plan = fixup(&grit, target, &opts).expect("plan");
    let new_oid = plan
        .commits
        .iter()
        .find(|r| r.old == target)
        .and_then(|r| r.new)
        .expect("new");
    let new = parse_commit(&grit.odb().read(&new_oid).expect("read").data).expect("parse");
    assert_eq!(new.message, "replacement subject\n");
}

#[test]
fn fixup_empty_policy_drop_keep_abort() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "first.t", "1\n", "first");
    commit_file(wt, "second.t", "2\n", "second");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "third.t", "3\n", "third");
    git(wt, &["rm", "second.t"]);
    let grit = open_grit(wt);
    let staged = staged_fixup_paths(&grit).expect("staged");
    let plan = fixup(
        &grit,
        target,
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    )
    .expect("drop plan");
    let row = plan
        .commits
        .iter()
        .find(|r| r.old == target)
        .expect("target row");
    assert!(
        row.new.is_none(),
        "drop should omit the empty target commit"
    );
    apply_fixup_rewrite(&grit, &plan, &apply_opts(), &staged).expect("drop apply");
    let log = git(wt, &["log", "--format=%s"]);
    assert!(!log.contains("second"));
    assert!(log.contains("third"));

    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    commit_file(wt2, "first.t", "1\n", "first");
    commit_file(wt2, "second.t", "2\n", "second");
    let target2 = git_rev_parse(wt2, "HEAD").expect("target");
    commit_file(wt2, "third.t", "3\n", "third");
    git(wt2, &["rm", "second.t"]);
    let grit2 = open_grit(wt2);
    let err = fixup(
        &grit2,
        target2,
        &fixup_opts(RefScope::Head, EmptyPolicy::Abort),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::FixupMakesTargetEmpty { .. }));

    let repo3 = HistoryRepo::init();
    let wt3 = repo3.worktree();
    commit_file(wt3, "first.t", "1\n", "first");
    commit_file(wt3, "second.t", "2\n", "second");
    let target3 = git_rev_parse(wt3, "HEAD").expect("target");
    commit_file(wt3, "third.t", "3\n", "third");
    git(wt3, &["rm", "second.t"]);
    let grit3 = open_grit(wt3);
    let staged3 = staged_fixup_paths(&grit3).expect("staged");
    let plan3 = fixup(
        &grit3,
        target3,
        &fixup_opts(RefScope::Head, EmptyPolicy::Keep),
    )
    .expect("keep plan");
    let row = plan3
        .commits
        .iter()
        .find(|r| r.old == target3)
        .expect("row");
    assert!(
        row.new.is_some(),
        "keep should retain an empty target commit"
    );
    apply_fixup_rewrite(&grit3, &plan3, &apply_opts(), &staged3).expect("keep apply");
}

#[test]
fn fixup_conflict_changes_nothing() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "base\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "base"]);
    fs::write(wt.join("file.txt"), "change\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "change"]);
    fs::write(wt.join("file.txt"), "conflict\n").expect("write");
    git(wt, &["add", "file.txt"]);
    let before = git_rev_parse(wt, "HEAD").expect("before");
    let grit = open_grit(wt);
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    let err = fixup(
        &grit,
        target,
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::Conflict { .. }));
    assert_eq!(git_rev_parse(wt, "HEAD"), Some(before));
}

#[test]
fn fixup_preserves_unstaged_deletion_after_partial_staging() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "committed\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "base"]);
    fs::write(wt.join("file.txt"), "staged\n").expect("write");
    git(wt, &["add", "file.txt"]);
    fs::remove_file(wt.join("file.txt")).expect("remove");
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        git_rev_parse(wt, "HEAD").expect("head"),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    assert!(
        !wt.join("file.txt").exists(),
        "worktree file must stay deleted"
    );
    let st = status(
        &grit,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert_eq!(st.staged.len(), 0, "staging consumed by fixup");
    assert_eq!(st.unstaged.len(), 1);
    assert_eq!(st.unstaged[0].status, DiffStatus::Deleted);
}

#[test]
fn fixup_preserves_unstaged_deletion_matches_git_fixup_autosquash() {
    if !support::skip_unless_git_at_least(2, 45) {
        return;
    }
    reset_harness_commit_clock();
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "committed\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "base"]);
    fs::write(wt.join("file.txt"), "staged\n").expect("write");
    git(wt, &["add", "file.txt"]);
    fs::remove_file(wt.join("file.txt")).expect("remove");
    disable_grit_change_ids_for_git_oracle(wt);
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        git_rev_parse(wt, "HEAD").expect("head"),
        &harness_fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    assert!(!wt.join("file.txt").exists());

    reset_harness_commit_clock();
    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    fs::write(wt2.join("file.txt"), "committed\n").expect("write");
    git(wt2, &["add", "file.txt"]);
    git(wt2, &["commit", "-m", "base"]);
    fs::write(wt2.join("file.txt"), "staged\n").expect("write");
    git(wt2, &["add", "file.txt"]);
    fs::remove_file(wt2.join("file.txt")).expect("remove");
    git_history_fixup_or_legacy(wt2, "HEAD");
    assert!(!wt2.join("file.txt").exists());
    assert_fixup_tip_matches_git_oracle(wt, wt2);
}

#[test]
fn fixup_preserves_unstaged_modification_after_partial_staging() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "one\nbase\nthree\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "base"]);
    fs::write(wt.join("file.txt"), "one\nstaged B\nthree\n").expect("write");
    git(wt, &["add", "file.txt"]);
    fs::write(wt.join("file.txt"), "one\nunstaged edit\nthree\n").expect("write");
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        git_rev_parse(wt, "HEAD").expect("head"),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    assert_eq!(
        fs::read_to_string(wt.join("file.txt")).expect("read"),
        "one\nunstaged edit\nthree\n"
    );
    let st = status(
        &grit,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert_eq!(st.staged.len(), 0);
    assert_eq!(st.unstaged.len(), 1);
}

#[test]
fn fixup_preserves_unstaged_insertion_on_partially_staged_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "one\nbase\nthree\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "base"]);
    fs::write(wt.join("file.txt"), "one\nstaged B\nthree\n").expect("write");
    git(wt, &["add", "file.txt"]);
    fs::write(wt.join("file.txt"), "one\nstaged B\nunstaged C\nthree\n").expect("write");
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        git_rev_parse(wt, "HEAD").expect("head"),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    assert_eq!(
        fs::read_to_string(wt.join("file.txt")).expect("read"),
        "one\nstaged B\nunstaged C\nthree\n"
    );
    let st = status(
        &grit,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert_eq!(st.staged.len(), 0);
    assert_eq!(st.unstaged.len(), 1);
}

#[test]
fn fixup_preserves_unstaged_suffix_on_partially_staged_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("file.txt"), "base\n").expect("write");
    git(wt, &["add", "file.txt"]);
    git(wt, &["commit", "-m", "base"]);
    fs::write(wt.join("file.txt"), "base\nstaged B\n").expect("write");
    git(wt, &["add", "file.txt"]);
    fs::write(wt.join("file.txt"), "base\nstaged B\nunstaged C\n").expect("write");
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        git_rev_parse(wt, "HEAD").expect("head"),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    assert_eq!(
        fs::read_to_string(wt.join("file.txt")).expect("read"),
        "base\nstaged B\nunstaged C\n"
    );
}

#[test]
fn fixup_preserves_unstaged_changes() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    fs::write(wt.join("a"), "").expect("touch");
    fs::write(wt.join("b"), "").expect("touch");
    git(wt, &["add", "."]);
    git(wt, &["commit", "-m", "initial commit"]);
    fs::write(wt.join("a"), "staged\n").expect("write");
    fs::write(wt.join("b"), "unstaged\n").expect("write");
    git(wt, &["add", "a"]);
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        git_rev_parse(wt, "HEAD").expect("head"),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    assert_eq!(
        fs::read_to_string(wt.join("b")).expect("read"),
        "unstaged\n"
    );
    let st = status(
        &grit,
        &StatusOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("status");
    assert_eq!(st.staged.len(), 0);
    assert_eq!(st.unstaged.len(), 1);
}

#[test]
fn fixup_nothing_staged_errors() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "only.t", "x\n", "only");
    let grit = open_grit(wt);
    let err = fixup(
        &grit,
        git_rev_parse(wt, "HEAD").expect("head"),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::NothingStaged));
}

#[test]
fn fixup_rejects_bare_repository() {
    let tmp = tempfile::tempdir().expect("tempdir");
    git(
        tmp.path(),
        &["init", "--bare", "-b", "main", tmp.path().to_str().unwrap()],
    );
    let grit = Repository::discover(Some(tmp.path()), &RepositoryOptions::empty()).expect("bare");
    let err = fixup(
        &grit,
        ObjectId::null(grit.odb().hash_algo()),
        &fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    )
    .unwrap_err();
    assert!(matches!(err, HistoryError::BareRepository));
}

#[test]
fn history_fixup_matches_git_fixup_oracle() {
    if !support::skip_unless_git_at_least(2, 45) {
        return;
    }
    reset_harness_commit_clock();
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "first.t", "1\n", "first");
    let target = commit_file(wt, "second.t", "2\n", "second");
    fs::write(wt.join("second.t"), "2\npatched\n").expect("write");
    git(wt, &["add", "second.t"]);
    disable_grit_change_ids_for_git_oracle(wt);
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        target,
        &harness_fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    reset_harness_commit_clock();
    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    commit_file(wt2, "first.t", "1\n", "first");
    commit_file(wt2, "second.t", "2\n", "second");
    fs::write(wt2.join("second.t"), "2\npatched\n").expect("write");
    git(wt2, &["add", "second.t"]);
    git_history_fixup_or_legacy(wt2, "HEAD");
    assert_fixup_tip_matches_git_oracle(wt, wt2);
}

#[test]
fn legacy_fixup_oracle_matches_grit_fixup_on_two_commit_history() {
    if !support::skip_unless_git_at_least(2, 45) {
        return;
    }
    reset_harness_commit_clock();
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "first.t", "1\n", "first");
    let target = commit_file(wt, "second.t", "2\n", "second");
    fs::write(wt.join("second.t"), "2\npatched\n").expect("write");
    git(wt, &["add", "second.t"]);
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        target,
        &harness_fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );

    reset_harness_commit_clock();
    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    commit_file(wt2, "first.t", "1\n", "first");
    commit_file(wt2, "second.t", "2\n", "second");
    fs::write(wt2.join("second.t"), "2\npatched\n").expect("write");
    git(wt2, &["add", "second.t"]);
    git_history_fixup_legacy_oracle(wt2, "HEAD");
    assert_fixup_tip_structure_matches(wt, wt2);
}

#[test]
fn history_fixup_matches_git_history_when_available() {
    if !git_history_subcommand_available("fixup") {
        eprintln!("SKIP: system git has no `history fixup` subcommand");
        return;
    }
    reset_harness_commit_clock();
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "file.txt", "base\n", "add file");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    fs::write(wt.join("file.txt"), "base\nfix\n").expect("write");
    git(wt, &["add", "file.txt"]);
    disable_grit_change_ids_for_git_oracle(wt);
    let grit = open_grit(wt);
    plan_and_apply(
        &grit,
        wt,
        target,
        &harness_fixup_opts(RefScope::Head, EmptyPolicy::Drop),
    );
    reset_harness_commit_clock();
    let repo2 = HistoryRepo::init();
    let wt2 = repo2.worktree();
    commit_file(wt2, "file.txt", "base\n", "add file");
    fs::write(wt2.join("file.txt"), "base\nfix\n").expect("write");
    git(wt2, &["add", "file.txt"]);
    git_history_fixup_or_legacy(wt2, "HEAD");
    assert_fixup_tip_matches_git_oracle(wt, wt2);
}

fn two_hunk_edited_file() -> (String, String) {
    let mut base = String::new();
    for i in 0..24 {
        base.push_str(&format!("line{i}\n"));
    }
    let mut edited = base.clone();
    edited = edited.replacen("line5\n", "LINE5\n", 1);
    edited = edited.replacen("line18\n", "LINE18\n", 1);
    (base, edited)
}

fn unstaged_hunk_ids(repo: &Repository) -> Vec<HunkId> {
    let files = unstaged_file_diffs(repo, &[]).expect("diff");
    assert_eq!(files.len(), 1);
    let (file, peer) = &files[0];
    hunk_ids_with_duplicate_peers(DiffSide::Unstaged, &file.path, &file.hunks, peer)
}

fn fixup_opts_with_selection(
    scope: RefScope,
    empty: EmptyPolicy,
    selection: HunkSelection,
) -> FixupOptions {
    FixupOptions {
        message: None,
        empty,
        rewrite: RewriteOptions {
            scope,
            empty,
            committer: committer(1).parse().expect("signature"),
            preserve_committer: false,
            sign: true,
        },
        selection: Some(selection),
        pathspecs: Vec::new(),
    }
}

fn plan_and_apply_hunks(
    grit: &Repository,
    wt: &std::path::Path,
    target: ObjectId,
    opts: &FixupOptions,
) {
    let selection = opts.selection.clone().expect("selection");
    let plan = fixup(grit, target, opts).expect("plan");
    apply_fixup_hunk_selection(grit, &plan, &apply_opts(), &selection, &opts.pathspecs)
        .expect("apply");
    git_fsck_strict(wt);
}

#[test]
fn fixup_hunks_folds_only_selected() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (base, edited) = two_hunk_edited_file();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    let ids = unstaged_hunk_ids(&grit);
    assert!(ids.len() >= 2);
    let selection = HunkSelection {
        ids: vec![ids[0].clone()],
        ranges: Vec::new(),
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let parent = git_rev_parse(wt, "HEAD~1").expect("parent");
    let fixed = parse_commit(&grit.odb().read(&parent).expect("read").data).expect("parse");
    let blob = grit.odb().read(&fixed.tree).expect("tree");
    let entries = grit_lib::objects::parse_tree(&blob.data).expect("parse tree");
    let file = entries.iter().find(|e| e.name == b"f.txt").expect("file");
    let blob = grit.odb().read(&file.oid).expect("blob");
    let content = String::from_utf8_lossy(&blob.data);
    assert!(content.contains("LINE5"));
    assert!(!content.contains("LINE18"));
    git(wt, &["fsck", "--strict"]);
    let log = Command::new("git")
        .current_dir(wt)
        .args(["log", "-p", "-1", &parent.to_hex()])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("log");
    assert!(log.status.success());
    let log_text = String::from_utf8_lossy(&log.stdout);
    assert!(log_text.contains("LINE5"));
    assert!(!log_text.contains("LINE18"));
}

#[test]
fn fixup_hunks_leaves_other_unstaged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (base, edited) = two_hunk_edited_file();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    let ids = unstaged_hunk_ids(&grit);
    let selection = HunkSelection {
        ids: vec![ids[0].clone()],
        ranges: Vec::new(),
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let remaining = unstaged_hunk_ids(&grit);
    assert_eq!(remaining.len(), 1);
    assert_ne!(remaining[0], ids[0]);
}

#[test]
fn fixup_hunks_preserves_preexisting_staged_on_same_path() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (base, edited) = two_hunk_edited_file();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    let ids = unstaged_hunk_ids(&grit);
    assert!(ids.len() >= 2);
    grit_lib::porcelain::hunk_select::stage_hunks(
        &grit,
        DiffSide::Unstaged,
        &HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        },
        &[],
        grit_lib::diff::DiffOptions::default(),
        false,
    )
    .expect("stage first hunk");
    let staged_before = Command::new("git")
        .current_dir(wt)
        .args(["diff", "--cached", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff cached");
    assert!(
        staged_before.status.success() && !staged_before.stdout.is_empty(),
        "line-5 hunk should be staged before fixup"
    );
    let selection = HunkSelection {
        ids: vec![ids[1].clone()],
        ranges: Vec::new(),
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let staged_after = Command::new("git")
        .current_dir(wt)
        .args(["diff", "--cached", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff cached after");
    assert!(
        staged_after.status.success(),
        "cached diff failed: {}",
        String::from_utf8_lossy(&staged_after.stderr)
    );
    let staged_text = String::from_utf8_lossy(&staged_after.stdout);
    assert!(
        staged_text.contains("LINE5"),
        "pre-existing staged line-5 edit must remain staged: {staged_text}"
    );
    assert!(
        !staged_text.contains("LINE18"),
        "consumed line-18 hunk must not remain staged: {staged_text}"
    );
    let status = Command::new("git")
        .current_dir(wt)
        .args(["status", "--short"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("status");
    assert!(status.status.success());
    let status_text = String::from_utf8_lossy(&status.stdout);
    assert!(
        status_text.contains("f.txt") && status_text.contains('M'),
        "expected staged f.txt, got: {status_text}"
    );
    git(wt, &["fsck", "--strict"]);
}

#[test]
fn fixup_lines_preserves_preexisting_staged_and_consumes_selected_line() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let (base, edited) = two_hunk_edited_file();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    let ids = unstaged_hunk_ids(&grit);
    assert!(ids.len() >= 2);
    grit_lib::porcelain::hunk_select::stage_hunks(
        &grit,
        DiffSide::Unstaged,
        &HunkSelection {
            ids: vec![ids[0].clone()],
            ranges: Vec::new(),
        },
        &[],
        grit_lib::diff::DiffOptions::default(),
        false,
    )
    .expect("stage line-5 hunk");
    let selection = HunkSelection {
        ids: Vec::new(),
        ranges: vec![LineRange {
            path: "f.txt".into(),
            start: 19,
            end: 19,
        }],
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let parent = git_rev_parse(wt, "HEAD~1").expect("parent");
    let fixed = parse_commit(&grit.odb().read(&parent).expect("read").data).expect("parse");
    let blob = grit.odb().read(&fixed.tree).expect("tree");
    let entries = grit_lib::objects::parse_tree(&blob.data).expect("parse tree");
    let file = entries.iter().find(|e| e.name == b"f.txt").expect("file");
    let blob = grit.odb().read(&file.oid).expect("blob");
    let content = String::from_utf8_lossy(&blob.data);
    assert!(content.contains("LINE18"));
    assert!(!content.contains("line18"));
    let staged_after = Command::new("git")
        .current_dir(wt)
        .args(["diff", "--cached", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff cached after");
    assert!(staged_after.status.success());
    let staged_text = String::from_utf8_lossy(&staged_after.stdout);
    assert!(
        staged_text.contains("LINE5"),
        "pre-existing staged line-5 edit must remain staged: {staged_text}"
    );
    assert!(
        !staged_text.contains("LINE18"),
        "consumed line-18 selection must not remain staged: {staged_text}"
    );
    git(wt, &["fsck", "--strict"]);
}

fn thirty_line_base() -> String {
    let mut base = String::new();
    for i in 0..30 {
        base.push_str(&format!("line{i}\n"));
    }
    base
}

#[test]
fn fixup_lines_same_structured_hunk_preserves_unselected_and_staged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base = thirty_line_base();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    let mut edited = base.clone();
    edited = edited.replacen("line5\n", "LINE5\n", 1);
    edited = edited.replacen("line7\n", "LINE7\n", 1);
    edited = edited.replacen("line18\n", "LINE18\n", 1);
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    grit_lib::porcelain::hunk_select::stage_hunks(
        &grit,
        DiffSide::Unstaged,
        &HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 19,
                end: 19,
            }],
        },
        &[],
        grit_lib::diff::DiffOptions::default(),
        false,
    )
    .expect("stage line-18 only");
    let selection = HunkSelection {
        ids: Vec::new(),
        ranges: vec![LineRange {
            path: "f.txt".into(),
            start: 6,
            end: 6,
        }],
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let parent = git_rev_parse(wt, "HEAD~1").expect("parent");
    let fixed = parse_commit(&grit.odb().read(&parent).expect("read").data).expect("parse");
    let blob = grit.odb().read(&fixed.tree).expect("tree");
    let entries = grit_lib::objects::parse_tree(&blob.data).expect("parse tree");
    let file = entries.iter().find(|e| e.name == b"f.txt").expect("file");
    let blob = grit.odb().read(&file.oid).expect("blob");
    let content = String::from_utf8_lossy(&blob.data);
    assert!(content.contains("LINE5"));
    assert!(!content.contains("LINE7"));
    assert!(!content.contains("LINE18"));
    let worktree = fs::read_to_string(wt.join("f.txt")).expect("read wt");
    assert!(worktree.contains("LINE7"));
    assert!(worktree.contains("LINE18"));
    let staged_after = Command::new("git")
        .current_dir(wt)
        .args(["diff", "--cached", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff cached after");
    assert!(staged_after.status.success());
    let staged_text = String::from_utf8_lossy(&staged_after.stdout);
    assert!(
        staged_text.contains("LINE18"),
        "pre-staged line-18 edit must remain staged: {staged_text}"
    );
    assert!(
        !staged_text.contains("LINE5"),
        "consumed line-5 selection must not remain staged: {staged_text}"
    );
    let unstaged_after = Command::new("git")
        .current_dir(wt)
        .args(["diff", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff unstaged after");
    assert!(unstaged_after.status.success());
    let unstaged_text = String::from_utf8_lossy(&unstaged_after.stdout);
    assert!(
        unstaged_text.contains("LINE7"),
        "unselected line-7 edit must stay unstaged: {unstaged_text}"
    );
    git(wt, &["fsck", "--strict"]);
}

#[test]
fn fixup_lines_pre_staged_same_hunk_preserves_unselected_and_staged() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base = thirty_line_base();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    let mut edited = base.clone();
    edited = edited.replacen("line5\n", "LINE5\n", 1);
    edited = edited.replacen("line7\n", "LINE7\n", 1);
    edited = edited.replacen("line9\n", "LINE9\n", 1);
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    grit_lib::porcelain::hunk_select::stage_hunks(
        &grit,
        DiffSide::Unstaged,
        &HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 6,
                end: 6,
            }],
        },
        &[],
        grit_lib::diff::DiffOptions::default(),
        false,
    )
    .expect("stage line-5 only");
    let selection = HunkSelection {
        ids: Vec::new(),
        ranges: vec![LineRange {
            path: "f.txt".into(),
            start: 8,
            end: 8,
        }],
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let parent = git_rev_parse(wt, "HEAD~1").expect("parent");
    let fixed = parse_commit(&grit.odb().read(&parent).expect("read").data).expect("parse");
    let blob = grit.odb().read(&fixed.tree).expect("tree");
    let entries = grit_lib::objects::parse_tree(&blob.data).expect("parse tree");
    let file = entries.iter().find(|e| e.name == b"f.txt").expect("file");
    let blob = grit.odb().read(&file.oid).expect("blob");
    let content = String::from_utf8_lossy(&blob.data);
    assert!(content.contains("LINE7"));
    assert!(!content.contains("LINE5"));
    assert!(!content.contains("LINE9"));
    let worktree = fs::read_to_string(wt.join("f.txt")).expect("read wt");
    assert!(worktree.contains("LINE5"));
    assert!(worktree.contains("LINE7"));
    assert!(worktree.contains("LINE9"));
    let staged_after = Command::new("git")
        .current_dir(wt)
        .args(["diff", "--cached", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff cached after");
    assert!(staged_after.status.success());
    let staged_text = String::from_utf8_lossy(&staged_after.stdout);
    assert!(
        staged_text.contains("LINE5"),
        "pre-staged line-5 edit must remain staged: {staged_text}"
    );
    let unstaged_after = Command::new("git")
        .current_dir(wt)
        .args(["diff", "f.txt"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("diff unstaged after");
    assert!(unstaged_after.status.success());
    let unstaged_text = String::from_utf8_lossy(&unstaged_after.stdout);
    assert!(
        unstaged_text.contains("LINE9"),
        "unselected line-9 edit must stay unstaged: {unstaged_text}"
    );
    git(wt, &["fsck", "--strict"]);
}

#[test]
fn fixup_hunks_stale_id_no_rewrite() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    commit_file(wt, "f.txt", "one\n", "init");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "tail.t", "t\n", "tail");
    fs::write(wt.join("f.txt"), "two\n").expect("write");
    let grit = open_grit(wt);
    let head_before = git_rev_parse(wt, "HEAD").expect("head");
    let selection = HunkSelection {
        ids: vec![HunkId::from_str("000000000001").expect("id")],
        ranges: Vec::new(),
    };
    let err = fixup(
        &grit,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    )
    .expect_err("stale");
    assert!(matches!(
        err,
        HistoryError::HunkSelect(HunkSelectError::StaleId { .. })
    ));
    assert_eq!(git_rev_parse(wt, "HEAD").expect("head"), head_before);
    git_fsck_strict(wt);
}

fn index_stage0_blob(wt: &std::path::Path, grit: &Repository, path: &str) -> Vec<u8> {
    let out = Command::new("git")
        .current_dir(wt)
        .args(["ls-files", "-s", "--", path])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .output()
        .expect("ls-files");
    assert!(out.status.success());
    let line = String::from_utf8_lossy(&out.stdout);
    let oid_hex = line.split_whitespace().nth(1).expect("index oid");
    let oid = ObjectId::from_hex(oid_hex.trim()).expect("parse oid");
    grit.odb().read(&oid).expect("blob").data
}

#[test]
fn fixup_hunk_insertion_shifts_remaining_unstaged_exact_bytes() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base = thirty_line_base();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    let mut edited = base.clone();
    edited = edited.replacen("line5\n", "NEW5\nINS1\nINS2\nINS3\n", 1);
    edited = edited.replacen("line20\n", "KEEP20\n", 1);
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    let selection = HunkSelection {
        ids: Vec::new(),
        ranges: vec![LineRange {
            path: "f.txt".into(),
            start: 6,
            end: 9,
        }],
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let mut expected_worktree = base.clone();
    expected_worktree = expected_worktree.replacen("line5\n", "NEW5\nINS1\nINS2\nINS3\n", 1);
    expected_worktree = expected_worktree.replacen("line20\n", "KEEP20\n", 1);
    assert_eq!(
        fs::read_to_string(wt.join("f.txt")).expect("worktree"),
        expected_worktree
    );
    let mut expected_index = base.clone();
    expected_index = expected_index.replacen("line5\n", "NEW5\nINS1\nINS2\nINS3\n", 1);
    assert_eq!(
        String::from_utf8_lossy(&index_stage0_blob(wt, &grit, "f.txt")),
        expected_index
    );
    git(wt, &["fsck", "--strict"]);
}

#[test]
fn fixup_lines_insertion_with_pre_staged_restores_shifted_unselected_below() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base = thirty_line_base();
    commit_file(wt, "f.txt", &base, "init");
    commit_file(wt, "other.t", "x\n", "second");
    let target = git_rev_parse(wt, "HEAD~1").expect("target");
    let mut edited = base.clone();
    edited = edited.replacen("line3\n", "STAGED3\n", 1);
    edited = edited.replacen("line5\n", "NEW5\nINS1\nINS2\nINS3\n", 1);
    edited = edited.replacen("line22\n", "KEEP22\n", 1);
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    grit_lib::porcelain::hunk_select::stage_hunks(
        &grit,
        DiffSide::Unstaged,
        &HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 4,
                end: 4,
            }],
        },
        &[],
        grit_lib::diff::DiffOptions::default(),
        false,
    )
    .expect("stage line-3");
    let selection = HunkSelection {
        ids: Vec::new(),
        ranges: vec![LineRange {
            path: "f.txt".into(),
            start: 6,
            end: 9,
        }],
    };
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let worktree = fs::read_to_string(wt.join("f.txt")).expect("worktree");
    assert!(
        worktree.contains("STAGED3") && worktree.contains("KEEP22") && worktree.contains("INS3")
    );
    git(wt, &["fsck", "--strict"]);
}

#[test]
fn fixup_hunk_selection_plans_against_index_worktree_base_non_head_target() {
    let repo = HistoryRepo::init();
    let wt = repo.worktree();
    let base = thirty_line_base();
    commit_file(wt, "f.txt", &base, "init");
    let target = git_rev_parse(wt, "HEAD").expect("target");
    commit_file(wt, "other.t", "x\n", "second");
    commit_file(wt, "other2.t", "y\n", "third");
    let mut edited = base.clone();
    edited = edited.replacen("line10\n", "FOLD10\n", 1);
    edited = edited.replacen("line25\n", "KEEP25\n", 1);
    fs::write(wt.join("f.txt"), &edited).expect("write");
    let grit = open_grit(wt);
    let files = unstaged_file_diffs(&grit, &[]).expect("diff");
    let (file, peer) = &files[0];
    let ids = hunk_ids_with_duplicate_peers(DiffSide::Unstaged, &file.path, &file.hunks, peer);
    let selection = HunkSelection {
        ids: vec![ids[0].clone()],
        ranges: Vec::new(),
    };
    grit_lib::porcelain::hunk_select::resolve_selection(
        &grit,
        DiffSide::Unstaged,
        &selection,
        &[],
        grit_lib::diff::DiffOptions::default(),
    )
    .expect("resolve");
    plan_and_apply_hunks(
        &grit,
        wt,
        target,
        &fixup_opts_with_selection(RefScope::Head, EmptyPolicy::Drop, selection),
    );
    let init = git_rev_parse(wt, "HEAD~2").expect("init");
    let init_commit = parse_commit(&grit.odb().read(&init).expect("read").data).expect("parse");
    let init_tree = grit.odb().read(&init_commit.tree).expect("tree");
    let init_entries = grit_lib::objects::parse_tree(&init_tree.data).expect("parse");
    let init_file = init_entries
        .iter()
        .find(|e| e.name == b"f.txt")
        .expect("file");
    let init_blob = grit.odb().read(&init_file.oid).expect("blob");
    let init_text = String::from_utf8_lossy(&init_blob.data);
    assert!(init_text.contains("FOLD10"));
    assert!(!init_text.contains("KEEP25"));
    assert!(fs::read_to_string(wt.join("f.txt"))
        .expect("wt")
        .contains("KEEP25"));
    git(wt, &["fsck", "--strict"]);
}
