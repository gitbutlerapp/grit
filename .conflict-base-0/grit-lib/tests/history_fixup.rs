//! Tests for [`grit_lib::revwalk::history::fixup`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

mod support;

use std::fs;

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::diff::DiffStatus;
use grit_lib::objects::{parse_commit, ObjectId};
use grit_lib::porcelain::status::{status, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use grit_lib::revwalk::history::{
    apply_fixup_rewrite, fixup, staged_fixup_paths, ApplyOptions, EmptyPolicy, FixupOptions,
    HistoryError, RefScope, RewriteOptions,
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
            committer: committer(1),
            preserve_committer: false,
            sign: true,
        },
    }
}

fn harness_fixup_opts(scope: RefScope, empty: EmptyPolicy) -> FixupOptions {
    FixupOptions {
        message: None,
        empty,
        rewrite: RewriteOptions {
            scope,
            empty,
            committer: harness_committer_line(),
            preserve_committer: false,
            sign: true,
        },
    }
}

fn apply_opts() -> ApplyOptions {
    ApplyOptions {
        identity: "Fixup <f@grit.test>".into(),
        reflog_message: "history fixup".into(),
        record_operation: None,
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
    let st = status(&grit, &StatusOptions::default(), &mut NullProgress).expect("status");
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
    let st = status(&grit, &StatusOptions::default(), &mut NullProgress).expect("status");
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
    let st = status(&grit, &StatusOptions::default(), &mut NullProgress).expect("status");
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
    let st = status(&grit, &StatusOptions::default(), &mut NullProgress).expect("status");
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
