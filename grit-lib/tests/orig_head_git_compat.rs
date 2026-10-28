//! Issue #2202: reset, merge and rebase record the pre-operation `HEAD` as `ORIG_HEAD`, the
//! way Git does, so `reset --hard ORIG_HEAD` undoes them. Checked with the system `git`.

use crate::support;

use std::path::Path;

use grit_lib::command_runner::{system_command_runner, RecordingRunner};
use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Error;
use grit_lib::index::write_tree::{write_tree_update_index, WriteTreeFlags};
use grit_lib::objects::{serialize_commit, CommitData, ObjectId, ObjectKind};
use grit_lib::porcelain::merge::{
    integrate_branch_with, plan_branch_merge, FastForwardPolicy, IntegrateBranchOutcome,
    MergeCommitMode, MergeOptions,
};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::progress::{NeverCancel, NullProgress};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::history::{
    abort_rebase, continue_rebase, start_rebase, start_rebase_with_exec, ApplyOptions, EmptyPolicy,
    ExecSpec, HistoryError, PreRebase, RebaseOntoOptions, RebaseOutcome, RebaseRequest,
    RebaseSessionError, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::state::{read_orig_head_for_repository, write_orig_head};
use grit_lib::{refs, RefStorageFormat};
use grit_test_support::{git, git_cmd};
use tempfile::TempDir;

const IDENT: &str = "T <t@e.c> 1700000000 +0000";

fn ident() -> grit_lib::ident::Signature {
    IDENT.parse().expect("signature")
}

fn open(dir: &Path) -> Repository {
    Repository::discover(Some(dir), &RepositoryOptions::empty()).expect("open repo")
}

fn rev(dir: &Path, spec: &str) -> ObjectId {
    grit_test_support::rev_parse(dir, spec)
}

/// `ORIG_HEAD` as the system `git` reads it.
fn git_orig_head(dir: &Path) -> Option<ObjectId> {
    let out = git_cmd(&["rev-parse", "--verify", "-q", "ORIG_HEAD"])
        .in_dir(dir)
        .exec();
    (out.status == Some(0)).then(|| ObjectId::from_hex(out.stdout.trim()).expect("oid"))
}

fn commit(dir: &Path, path: &str, contents: &str, msg: &str) -> ObjectId {
    std::fs::write(dir.join(path), contents).expect("write");
    git(dir, &["add", path]);
    git(dir, &["commit", "-qm", msg]);
    rev(dir, "HEAD")
}

/// The issue's repository: `main` at c1-c2-c3 and `topic` adding `t` on c1, on `main`.
fn issue_repo() -> (TempDir, ObjectId, ObjectId) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@e.c"]);
    for i in 1..=3 {
        commit(dir, &format!("f{i}"), &format!("{i}\n"), &format!("c{i}"));
    }
    git(dir, &["switch", "-q", "-c", "topic", "HEAD~2"]);
    let topic = commit(dir, "t", "t\n", "topic");
    git(dir, &["switch", "-q", "main"]);
    let main = rev(dir, "main");
    (tmp, main, topic)
}

fn set_stale_orig_head(dir: &Path) -> ObjectId {
    let stale = rev(dir, "main~2");
    git(dir, &["update-ref", "ORIG_HEAD", &stale.to_hex()]);
    stale
}

fn reset(repo: &Repository, target: ObjectId, mode: ResetMode) -> Result<(), Error> {
    reset_to_commit(
        repo,
        &ResetOptions::new(target, mode, ident(), "reset: moving"),
        &mut NullProgress,
        &NeverCancel,
    )
    .map(|_| ())
}

fn merge(
    repo: &Repository,
    into: ObjectId,
    other: ObjectId,
    options: &MergeOptions,
) -> Result<IntegrateBranchOutcome, Error> {
    integrate_branch_with(
        repo,
        "refs/heads/main",
        into,
        other,
        "topic",
        options,
        &mut NullProgress,
        &NeverCancel,
    )
}

fn merge_options() -> MergeOptions {
    MergeOptions {
        message: Some("merge topic".into()),
        ..MergeOptions::default()
    }
}

/// `ORIG_HEAD` is the pre-operation `HEAD` for both grit and git, `git fsck` is clean, and
/// resetting to it with grit restores that `HEAD`.
fn assert_orig_head_undoes(dir: &Path, before: ObjectId) {
    let repo = open(dir);
    assert_eq!(git_orig_head(dir), Some(before), "git reads ORIG_HEAD");
    assert_eq!(
        read_orig_head_for_repository(&repo).expect("read"),
        Some(before)
    );
    assert!(
        !dir.join(".git/logs/ORIG_HEAD").exists(),
        "ORIG_HEAD has no reflog"
    );
    git(dir, &["fsck", "--strict"]);
    let target = resolve_revision(&repo, "ORIG_HEAD").expect("ORIG_HEAD resolves");
    assert_eq!(target, before);
    reset(&repo, target, ResetMode::Hard).expect("reset --hard ORIG_HEAD");
    assert_eq!(rev(dir, "HEAD"), before);
    assert_eq!(git(dir, &["status", "--porcelain"]), "");
}

#[test]
fn every_reset_mode_records_previous_head_over_a_stale_value() {
    for mode in [
        ResetMode::Soft,
        ResetMode::Mixed,
        ResetMode::Hard,
        ResetMode::Keep,
    ] {
        let (tmp, main, _) = issue_repo();
        let dir = tmp.path();
        set_stale_orig_head(dir);
        let repo = open(dir);
        reset(&repo, rev(dir, "HEAD~1"), mode).expect("reset");
        assert_eq!(git_orig_head(dir), Some(main), "{mode:?}");
        if mode == ResetMode::Hard {
            assert_orig_head_undoes(dir, main);
        }
    }
}

#[test]
fn reset_to_head_and_detached_reset_record_orig_head_like_git() {
    let (tmp, main, _) = issue_repo();
    let dir = tmp.path();
    set_stale_orig_head(dir);
    reset(&open(dir), main, ResetMode::Mixed).expect("reset to HEAD");
    assert_eq!(git_orig_head(dir), Some(main));

    git(dir, &["switch", "-q", "--detach", "main"]);
    let target = rev(dir, "HEAD~2");
    reset(&open(dir), target, ResetMode::Hard).expect("detached reset");
    assert_eq!(git_orig_head(dir), Some(main));

    let (git_tmp, git_main, _) = issue_repo();
    git(git_tmp.path(), &["switch", "-q", "--detach", "main"]);
    git(git_tmp.path(), &["reset", "-q", "--hard", "HEAD~2"]);
    assert_eq!(git_orig_head(git_tmp.path()), Some(git_main));
}

#[test]
fn dry_run_and_refused_resets_leave_orig_head_alone() {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    let repo = open(dir);
    let target = rev(dir, "HEAD~1");
    let mut opts = ResetOptions::new(target, ResetMode::Hard, ident(), "reset");
    opts.dry_run = true;
    reset_to_commit(&repo, &opts, &mut NullProgress, &NeverCancel).expect("dry run");
    assert_eq!(
        git_orig_head(dir),
        None,
        "dry run must not create ORIG_HEAD"
    );

    let stale = set_stale_orig_head(dir);
    std::fs::write(dir.join("f3"), "local edit\n").expect("edit");
    let err = reset(&repo, target, ResetMode::Keep).expect_err("keep refuses");
    assert!(matches!(err, Error::WouldOverwrite { .. }), "{err:?}");
    assert_eq!(git_orig_head(dir), Some(stale));
}

#[test]
fn issue_2202_merge_then_reset_to_orig_head_undoes_it() {
    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    set_stale_orig_head(dir);
    let outcome = merge(&open(dir), main, topic, &merge_options()).expect("merge");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Merged { .. }),
        "{outcome:?}"
    );
    assert_orig_head_undoes(dir, main);
}

#[test]
fn fast_forward_squash_and_up_to_date_merges_record_orig_head() {
    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    let repo = open(dir);
    let behind = rev(dir, "main~2");
    let outcome = merge(&repo, main, behind, &merge_options()).expect("up to date");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::UpToDate),
        "{outcome:?}"
    );
    assert_eq!(git_orig_head(dir), Some(main), "up-to-date merge");

    git(dir, &["reset", "-q", "--hard", "main~2"]);
    let ff = merge(&repo, behind, main, &merge_options()).expect("fast-forward");
    assert!(
        matches!(ff, IntegrateBranchOutcome::FastForward { .. }),
        "{ff:?}"
    );
    assert_orig_head_undoes(dir, behind);

    git(dir, &["reset", "-q", "--hard", &main.to_hex()]);
    let squash = MergeOptions {
        commit: MergeCommitMode::Squash,
        ..merge_options()
    };
    let outcome = merge(&repo, main, topic, &squash).expect("squash");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Squashed),
        "{outcome:?}"
    );
    assert_orig_head_undoes(dir, main);
}

#[test]
fn refused_ff_only_merge_still_records_orig_head_like_git() {
    let (git_tmp, git_main, _) = issue_repo();
    let _ = git_cmd(&["merge", "--ff-only", "topic"])
        .in_dir(git_tmp.path())
        .with_status(128);
    assert_eq!(git_orig_head(git_tmp.path()), Some(git_main));

    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    set_stale_orig_head(dir);
    let only = MergeOptions {
        fast_forward: FastForwardPolicy::Only,
        ..merge_options()
    };
    let err = merge(&open(dir), main, topic, &only).expect_err("not a fast-forward");
    assert!(matches!(err, Error::NotFastForward), "{err:?}");
    assert_eq!(git_orig_head(dir), Some(main));
    assert_eq!(rev(dir, "HEAD"), main);
}

#[test]
fn conflicted_merge_records_orig_head_and_abort_keeps_it() {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    git(dir, &["switch", "-q", "topic"]);
    let topic = commit(dir, "f1", "topic side\n", "topic edits f1");
    git(dir, &["switch", "-q", "main"]);
    let main = commit(dir, "f1", "main side\n", "main edits f1");
    set_stale_orig_head(dir);
    let repo = open(dir);
    let outcome = merge(&repo, main, topic, &merge_options()).expect("merge");
    assert!(
        matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }),
        "{outcome:?}"
    );
    assert_eq!(git_orig_head(dir), Some(main));
    grit_lib::porcelain::merge::abort_branch_merge(&repo).expect("abort");
    assert_eq!(git_orig_head(dir), Some(main));
    assert_eq!(rev(dir, "HEAD"), main);
}

#[test]
fn merge_dry_run_plan_does_not_create_orig_head() {
    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    plan_branch_merge(&open(dir), main, topic, "topic", &merge_options()).expect("plan");
    assert_eq!(git_orig_head(dir), None);
    assert!(!dir.join(".git/ORIG_HEAD").exists());
}

fn rebase_opts(upstream: ObjectId, tip: ObjectId) -> StartRebaseOptions {
    StartRebaseOptions {
        request: RebaseRequest {
            upstream,
            onto: None,
            branch_tip: tip,
        },
        rebase: RebaseOntoOptions {
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: IDENT.parse().expect("signature"),
                preserve_committer: false,
                sign: false,
            },
            skip_applied: false,
            flatten_leaf_merges: false,
            favor: grit_lib::diff::merge_file::MergeFavor::default(),
            drop: Vec::new(),
        },
        apply: ApplyOptions {
            identity: ident(),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Skip,
        },
        pre_rebase: PreRebase::Skip,
    }
}

fn rebase(dir: &Path, upstream: ObjectId, tip: ObjectId) -> RebaseOutcome {
    start_rebase(
        &open(dir),
        &rebase_opts(upstream, tip),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("start rebase")
}

#[test]
fn issue_2202_clean_rebase_then_reset_to_orig_head_undoes_it() {
    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    git(dir, &["switch", "-q", "topic"]);
    set_stale_orig_head(dir);
    let outcome = rebase(dir, main, topic);
    assert!(matches!(outcome, RebaseOutcome::Done { .. }), "{outcome:?}");
    assert_ne!(rev(dir, "HEAD"), topic);
    assert_orig_head_undoes(dir, topic);
}

#[test]
fn up_to_date_rebase_leaves_orig_head_untouched() {
    let (tmp, main, _) = issue_repo();
    let dir = tmp.path();
    let outcome = rebase(dir, rev(dir, "main~1"), main);
    assert!(matches!(outcome, RebaseOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        git_orig_head(dir),
        None,
        "nothing to rebase creates nothing"
    );
    let stale = set_stale_orig_head(dir);
    rebase(dir, rev(dir, "main~1"), main);
    assert_eq!(git_orig_head(dir), Some(stale));
}

#[test]
fn up_to_date_rebase_with_exec_runs_at_each_commit_without_rewriting_tip() {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    let tip_before = rev(dir, "HEAD");
    let upstream = rev(dir, "HEAD~2");
    let runner = RecordingRunner::always_success();
    let outcome = start_rebase_with_exec(
        &open(dir),
        &rebase_opts(upstream, tip_before),
        &ExecSpec {
            commands: vec!["true".into()],
        },
        runner.as_ref(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("exec rebase");
    assert!(
        matches!(outcome, RebaseOutcome::ExecDone { .. }),
        "{outcome:?}"
    );
    assert_eq!(runner.specs().len(), 2, "c2 and c3");
    assert_eq!(rev(dir, "HEAD"), tip_before);
    assert_eq!(git_orig_head(dir), Some(tip_before));
    git(dir, &["fsck", "--strict"]);
}

#[test]
fn up_to_date_rebase_with_exec_amend_moves_stacked_branch() {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    git(dir, &["switch", "-q", "main"]);
    let main_before = rev(dir, "HEAD");
    git(dir, &["switch", "-q", "-c", "stacked"]);
    let stacked_before = commit(dir, "stack.txt", "s\n", "stacked");
    git(dir, &["switch", "-q", "main"]);
    assert_ne!(stacked_before, main_before);
    assert_eq!(rev(dir, "main"), main_before);
    let upstream = rev(dir, "HEAD~2");
    let mut opts = rebase_opts(upstream, main_before);
    opts.rebase.rewrite.scope = RefScope::Branches;
    let amend = "env GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null \
        GIT_AUTHOR_NAME=T GIT_AUTHOR_EMAIL=t@e.c GIT_COMMITTER_NAME=T GIT_COMMITTER_EMAIL=t@e.c \
        git commit -q --amend -m amended";
    let outcome = start_rebase_with_exec(
        &open(dir),
        &opts,
        &ExecSpec {
            commands: vec![amend.into()],
        },
        system_command_runner().as_ref(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("exec rebase");
    assert!(
        matches!(outcome, RebaseOutcome::ExecDone { .. }),
        "{outcome:?}"
    );
    let main_after = rev(dir, "main");
    let stacked_after = rev(dir, "refs/heads/stacked");
    assert_ne!(main_after, main_before);
    assert_ne!(stacked_after, stacked_before);
    assert_eq!(rev(dir, "stacked^"), main_after);
    git(dir, &["fsck", "--strict"]);
}

#[test]
fn up_to_date_rebase_with_exec_failure_leaves_refs_unchanged() {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    let tip_before = rev(dir, "HEAD");
    let upstream = rev(dir, "HEAD~2");
    let err = rebase_with_exec(dir, upstream, tip_before, "false").expect_err("command fails");
    assert!(
        matches!(
            err,
            RebaseSessionError::History(HistoryError::ExecFailed { .. })
        ),
        "{err:?}"
    );
    assert_eq!(rev(dir, "HEAD"), tip_before);
    assert_eq!(git_orig_head(dir), None);
}

#[test]
fn up_to_date_rebase_with_exec_on_empty_range_runs_nothing() {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    let tip = rev(dir, "HEAD");
    let runner = RecordingRunner::always_success();
    let outcome = start_rebase_with_exec(
        &open(dir),
        &rebase_opts(tip, tip),
        &ExecSpec {
            commands: vec!["true".into()],
        },
        runner.as_ref(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("exec");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }), "{outcome:?}");
    assert!(runner.specs().is_empty());
}

fn rebase_with_exec(
    dir: &Path,
    upstream: ObjectId,
    tip: ObjectId,
    command: &str,
) -> Result<RebaseOutcome, RebaseSessionError> {
    start_rebase_with_exec(
        &open(dir),
        &rebase_opts(upstream, tip),
        &ExecSpec {
            commands: vec![command.to_owned()],
        },
        system_command_runner().as_ref(),
        &mut NullProgress,
        &NeverCancel,
    )
}

#[test]
fn exec_rebase_records_orig_head_like_git() {
    let (git_tmp, _, git_topic) = issue_repo();
    git(git_tmp.path(), &["switch", "-q", "topic"]);
    git(git_tmp.path(), &["rebase", "-q", "main", "--exec", "true"]);
    assert_eq!(git_orig_head(git_tmp.path()), Some(git_topic));

    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    git(dir, &["switch", "-q", "topic"]);
    set_stale_orig_head(dir);
    let outcome = rebase_with_exec(dir, main, topic, "true").expect("exec rebase");
    assert!(
        matches!(outcome, RebaseOutcome::ExecDone { .. }),
        "{outcome:?}"
    );
    assert_ne!(rev(dir, "HEAD"), topic);
    assert_orig_head_undoes(dir, topic);
}

#[test]
fn exec_rebase_stop_records_orig_head_and_failure_leaves_it_alone() {
    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    git(dir, &["switch", "-q", "topic"]);
    let err = rebase_with_exec(dir, main, topic, "exit 7").expect_err("command fails");
    assert!(
        matches!(
            err,
            RebaseSessionError::History(HistoryError::ExecFailed { .. })
        ),
        "{err:?}"
    );
    assert_eq!(
        git_orig_head(dir),
        None,
        "a rolled-back exec rebase records nothing"
    );
    assert_eq!(rev(dir, "HEAD"), topic);

    set_stale_orig_head(dir);
    let err = rebase_with_exec(dir, main, topic, "echo edit >> t").expect_err("dirty stop");
    assert!(
        matches!(
            err,
            RebaseSessionError::History(HistoryError::ExecDirty { .. })
        ),
        "{err:?}"
    );
    assert_eq!(git_orig_head(dir), Some(topic), "exec stop");
    abort_rebase(&open(dir), &ident()).expect("abort");
    assert_eq!(rev(dir, "HEAD"), topic);
    assert_eq!(git_orig_head(dir), Some(topic), "after abort");
    git(dir, &["fsck", "--strict"]);
}

/// `issue_repo` where `topic` and `main` both edit `f1`, checked out on `topic`.
fn conflicting_topic_repo() -> (TempDir, ObjectId, ObjectId) {
    let (tmp, _, _) = issue_repo();
    let dir = tmp.path();
    let main = commit(dir, "f1", "main side\n", "main edits f1");
    git(dir, &["switch", "-q", "topic"]);
    let topic = commit(dir, "f1", "topic side\n", "topic edits f1");
    (tmp, main, topic)
}

#[test]
fn favored_rebase_records_orig_head_like_git() {
    let (git_tmp, git_main, git_topic) = conflicting_topic_repo();
    git(
        git_tmp.path(),
        &["rebase", "-q", "-X", "ours", &git_main.to_hex()],
    );
    assert_eq!(git_orig_head(git_tmp.path()), Some(git_topic));

    let (tmp, main, topic) = conflicting_topic_repo();
    let dir = tmp.path();
    set_stale_orig_head(dir);
    let mut opts = rebase_opts(main, topic);
    opts.rebase.favor = grit_lib::diff::merge_file::MergeFavor::Ours;
    let outcome =
        start_rebase(&open(dir), &opts, &mut NullProgress, &NeverCancel).expect("favored rebase");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }), "{outcome:?}");
    assert_eq!(
        git(dir, &["show", "HEAD:f1"]),
        git(git_tmp.path(), &["show", "HEAD:f1"]),
        "-X ours keeps the upstream side, as git does"
    );
    assert_orig_head_undoes(dir, topic);
}

/// `topic` first deletes `f1`, which `main` modified (a conflict `-X` cannot resolve), then
/// edits `g`, which `main` also edited. Checked out on `topic`; returns `(main, topic)`.
fn favor_then_continue_repo() -> (TempDir, ObjectId, ObjectId) {
    let tmp = TempDir::new().expect("tempdir");
    let dir = tmp.path();
    git(dir, &["init", "-q", "-b", "main"]);
    git(dir, &["config", "user.name", "T"]);
    git(dir, &["config", "user.email", "t@e.c"]);
    std::fs::write(dir.join("g"), "base g\n").expect("write g");
    commit(dir, "f1", "base\n", "base");
    git(dir, &["add", "g"]);
    git(dir, &["commit", "-qm", "base g"]);
    git(dir, &["switch", "-q", "-c", "topic"]);
    git(dir, &["rm", "-q", "f1"]);
    git(dir, &["commit", "-qm", "topic drops f1"]);
    let topic = commit(dir, "g", "topic g\n", "topic edits g");
    git(dir, &["switch", "-q", "main"]);
    commit(dir, "f1", "main f1\n", "main edits f1");
    let main = commit(dir, "g", "main g\n", "main edits g");
    git(dir, &["switch", "-q", "topic"]);
    (tmp, main, topic)
}

#[test]
fn favored_rebase_stops_on_unresolvable_conflict_and_continue_keeps_the_favor() {
    let (git_tmp, git_main, git_topic) = favor_then_continue_repo();
    let git_dir = git_tmp.path();
    let _ = git_cmd(&["rebase", "-q", "-X", "ours", &git_main.to_hex()])
        .in_dir(git_dir)
        .with_status(1);
    git(git_dir, &["rm", "-q", "f1"]);
    git_cmd(&["-c", "core.editor=true", "rebase", "--continue"])
        .in_dir(git_dir)
        .suc();
    assert_eq!(git_orig_head(git_dir), Some(git_topic));

    let (tmp, main, topic) = favor_then_continue_repo();
    let dir = tmp.path();
    let mut opts = rebase_opts(main, topic);
    opts.rebase.favor = grit_lib::diff::merge_file::MergeFavor::Ours;
    let repo = open(dir);
    let outcome = start_rebase(&repo, &opts, &mut NullProgress, &NeverCancel).expect("start");
    assert!(matches!(outcome, RebaseOutcome::Stopped(_)), "{outcome:?}");
    assert_eq!(git_orig_head(dir), Some(topic));
    git(dir, &["rm", "-q", "f1"]);
    let done = continue_rebase(&repo, &opts, &mut NullProgress, &NeverCancel).expect("continue");
    assert!(matches!(done, RebaseOutcome::Done { .. }), "{done:?}");
    assert_eq!(
        git(dir, &["show", "HEAD:g"]),
        git(git_dir, &["show", "HEAD:g"])
    );
    assert_eq!(git(dir, &["show", "HEAD:g"]), "main g\n");
    assert_orig_head_undoes(dir, topic);
}

#[test]
fn stopped_rebase_keeps_orig_head_through_continue_and_abort() {
    for finish in ["continue", "abort"] {
        let (tmp, main, _) = issue_repo();
        let dir = tmp.path();
        git(dir, &["switch", "-q", "topic"]);
        let tip = commit(dir, "f2", "topic side\n", "topic edits f2");
        set_stale_orig_head(dir);
        let outcome = rebase(dir, main, tip);
        assert!(matches!(outcome, RebaseOutcome::Stopped(_)), "{outcome:?}");
        assert_eq!(git_orig_head(dir), Some(tip), "stopped rebase");

        let repo = open(dir);
        if finish == "continue" {
            std::fs::write(dir.join("f2"), "resolved\n").expect("resolve");
            git(dir, &["add", "f2"]);
            let done = continue_rebase(
                &repo,
                &rebase_opts(main, tip),
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("continue");
            assert!(matches!(done, RebaseOutcome::Done { .. }), "{done:?}");
        } else {
            abort_rebase(&repo, &ident()).expect("abort");
            assert_eq!(rev(dir, "HEAD"), tip);
        }
        assert_eq!(git_orig_head(dir), Some(tip), "after {finish}");
        git(dir, &["fsck", "--strict"]);
    }
}

/// Commit `f` = `contents` on `main` through the library, so any ref backend works.
fn commit_on_main(repo: &Repository, contents: &str, parent: Option<ObjectId>) -> ObjectId {
    let wt = repo.work_tree().expect("work tree");
    std::fs::write(wt.join("f"), contents).expect("write");
    let blob = repo
        .odb()
        .write(ObjectKind::Blob, contents.as_bytes())
        .expect("blob");
    let mut index = repo.index_handle().load().expect("index");
    index.add_or_replace(
        grit_lib::index::entry_from_stat(&wt.join("f"), b"f", blob, grit_lib::index::MODE_REGULAR)
            .expect("entry"),
    );
    index.sort();
    let tree = write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
        .expect("tree");
    repo.index_handle().write(&mut index).expect("write index");
    let commit = CommitData {
        tree,
        parents: parent.into_iter().collect(),
        author: IDENT.into(),
        committer: IDENT.into(),
        author_raw: vec![],
        committer_raw: vec![],
        encoding: None,
        message: "c\n".into(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let oid = repo
        .odb()
        .write(ObjectKind::Commit, &serialize_commit(&commit))
        .expect("commit");
    refs::update_direct_ref_on_store(repo.refs(), repo.git_dir(), "refs/heads/main", &oid)
        .expect("branch");
    oid
}

#[test]
fn reftable_reset_stores_orig_head_in_the_ref_store() {
    let tmp = TempDir::new().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Reftable),
    )
    .expect("init reftable");
    let first = commit_on_main(&repo, "1\n", None);
    let second = commit_on_main(&repo, "2\n", Some(first));
    reset(&repo, first, ResetMode::Hard).expect("reset");
    assert!(
        !repo.git_dir().join("ORIG_HEAD").exists(),
        "reftable keeps ORIG_HEAD in the stack, not a loose file"
    );
    assert_eq!(
        read_orig_head_for_repository(&repo).expect("read"),
        Some(second)
    );
    assert_eq!(resolve_revision(&repo, "ORIG_HEAD").expect("rev"), second);
    if support::git_supports_reftable() {
        assert_eq!(git_orig_head(tmp.path()), Some(second));
        git(tmp.path(), &["fsck", "--strict"]);
    }
}

#[test]
fn write_orig_head_overwrites_and_git_reads_it() {
    let (tmp, main, topic) = issue_repo();
    let dir = tmp.path();
    let repo = open(dir);
    assert_eq!(read_orig_head_for_repository(&repo).expect("read"), None);
    write_orig_head(&repo, topic).expect("write");
    assert_eq!(git_orig_head(dir), Some(topic));
    write_orig_head(&repo, main).expect("overwrite");
    assert_eq!(git_orig_head(dir), Some(main));
    assert_eq!(
        read_orig_head_for_repository(&repo).expect("read"),
        Some(main)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join(".git/ORIG_HEAD")).expect("loose file"),
        format!("{}\n", main.to_hex())
    );
}
