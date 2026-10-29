//! Staging, commit, stash, merge, rebase, replay and restore report progress and stop on
//! cancellation (issue #2142), through either the `&dyn Cancel` argument or
//! [`ProgressSink::should_cancel`].

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::fs;
use std::path::Path;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::porcelain::add::{stage, StageOptions};
use grit_lib::porcelain::commit::{commit_with_scope, CommitRequest, CommitScope};
use grit_lib::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
use grit_lib::porcelain::replay::{replay_commit, ReplayDirection, ReplayRequest};
use grit_lib::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreSource};
use grit_lib::progress::{
    Cancel, FlagCancel, NeverCancel, NullProgress, ProgressPhase, ProgressSink,
};
use grit_lib::repo::{InitOptions, Repository};
use grit_lib::revwalk::history::{
    continue_rebase, start_rebase, ApplyOptions, EmptyPolicy, RebaseOntoOptions, RebaseOutcome,
    RebaseRequest, RebaseSessionError, RefScope, RewriteOptions, StartRebaseOptions,
};
use grit_lib::stash::{StashApplyOptions, StashMode, StashPushOptions};
use grit_lib::RefStorageFormat;

const IDENT: &str = "Cancel Test <cancel@example.com> 1700000000 +0000";
const FILES: usize = 300;

/// Records every phase and unit count, and asks to stop once `limit` units of `phase` passed.
struct CancelAfter {
    phase: Option<ProgressPhase>,
    limit: u64,
    /// When set, cancel as soon as this phase calls [`ProgressSink::finish`].
    cancel_when_phase_finishes: Option<ProgressPhase>,
    active: Option<ProgressPhase>,
    seen: u64,
    started: Vec<ProgressPhase>,
    flag: FlagCancel,
}

impl CancelAfter {
    /// Stops after `limit` units of `phase`, or as soon as `phase` starts when `limit` is 0.
    fn new(phase: ProgressPhase, limit: u64) -> Self {
        Self {
            phase: Some(phase),
            limit,
            cancel_when_phase_finishes: None,
            active: None,
            seen: 0,
            started: Vec::new(),
            flag: FlagCancel::new(),
        }
    }

    /// Cancel at the first [`ProgressSink::finish`] after this phase ran (after checkout).
    fn cancel_when_phase_finishes(phase: ProgressPhase) -> Self {
        Self {
            phase: None,
            limit: 0,
            cancel_when_phase_finishes: Some(phase),
            active: None,
            seen: 0,
            started: Vec::new(),
            flag: FlagCancel::new(),
        }
    }

    /// Never asks to stop; only records.
    fn recorder() -> Self {
        Self {
            phase: None,
            cancel_when_phase_finishes: None,
            ..Self::new(ProgressPhase::StagingFiles, 0)
        }
    }

    fn units_of(&self, phase: ProgressPhase) -> u64 {
        if self.started.contains(&phase) {
            self.seen
        } else {
            0
        }
    }
}

impl ProgressSink for CancelAfter {
    fn start(&mut self, phase: ProgressPhase, _total: Option<u64>) {
        self.active = Some(phase);
        self.started.push(phase);
        if self.limit == 0 && self.phase == Some(phase) {
            self.flag.cancel();
        }
    }

    fn inc(&mut self, units: u64) {
        if self.active.is_none() {
            return;
        }
        self.seen += units;
        if self.phase.is_some() && self.phase == self.active && self.seen >= self.limit {
            self.flag.cancel();
        }
    }

    fn finish(&mut self) {
        if self.cancel_when_phase_finishes.is_some()
            && self.cancel_when_phase_finishes == self.active
        {
            self.flag.cancel();
        }
    }

    fn should_cancel(&self) -> bool {
        self.flag.is_cancelled()
    }
}

fn cancelled_flag() -> FlagCancel {
    let flag = FlagCancel::new();
    flag.cancel();
    flag
}

fn init_repo() -> (tempfile::TempDir, Repository) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let repo = Repository::init(
        tmp.path(),
        &InitOptions::new()
            .with_initial_branch("main".to_owned())
            .with_ref_storage(RefStorageFormat::Files),
    )
    .expect("init");
    (tmp, repo)
}

fn commit_req(message: &str) -> CommitRequest {
    grit_test_support::commit_request(message, IDENT)
}

fn write_files(root: &Path, dir: &str, contents: &str) {
    fs::create_dir_all(root.join(dir)).expect("mkdir");
    for i in 0..FILES {
        fs::write(
            root.join(format!("{dir}/f{i:04}.txt")),
            format!("{contents} {i}\n"),
        )
        .expect("write");
    }
}

/// Stage everything and commit it, with no progress and no cancel.
fn commit_all(repo: &Repository, message: &str) -> ObjectId {
    commit_with_scope(
        repo,
        &commit_req(message),
        CommitScope::AutoStage,
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("commit")
    .oid
}

fn head(repo: &Repository) -> ObjectId {
    grit_lib::refs::resolve_ref(repo.git_dir(), "HEAD").expect("HEAD")
}

fn branch(repo: &Repository, name: &str) -> ObjectId {
    grit_lib::refs::resolve_ref(repo.git_dir(), &format!("refs/heads/{name}")).expect("branch")
}

fn index_bytes(repo: &Repository) -> Vec<u8> {
    fs::read(repo.git_dir().join("index")).unwrap_or_default()
}

fn assert_cancelled<T: std::fmt::Debug>(result: grit_lib::error::Result<T>) {
    match result {
        Err(Error::Cancelled) => {}
        other => panic!("expected Error::Cancelled, got {other:?}"),
    }
}

fn assert_index_untouched(repo: &Repository, before: &[u8]) {
    assert_eq!(index_bytes(repo), before, "on-disk index must be unchanged");
    assert!(
        !repo.git_dir().join("index.lock").exists(),
        "index.lock must be released"
    );
}

/// Repository with one commit of [`FILES`] tracked files, every one of them modified since.
fn repo_with_modified_files() -> (tempfile::TempDir, Repository, ObjectId) {
    let (tmp, repo) = init_repo();
    write_files(tmp.path(), "d", "base");
    let base = commit_all(&repo, "base");
    write_files(tmp.path(), "d", "changed");
    (tmp, repo, base)
}

#[test]
fn stage_reports_staging_progress() {
    let (tmp, repo) = init_repo();
    write_files(tmp.path(), "d", "new");
    let mut sink = CancelAfter::recorder();
    let outcome = stage(&repo, &StageOptions::default(), &mut sink, &NeverCancel).expect("stage");
    assert_eq!(outcome.added.len(), FILES);
    assert_eq!(sink.started.first(), Some(&ProgressPhase::StagingFiles));
    assert_eq!(sink.units_of(ProgressPhase::StagingFiles), FILES as u64);
}

#[test]
fn stage_stops_when_the_sink_asks() {
    let (tmp, repo) = init_repo();
    write_files(tmp.path(), "d", "new");
    let before = index_bytes(&repo);
    let mut sink = CancelAfter::new(ProgressPhase::StagingFiles, 1);
    assert_cancelled(stage(
        &repo,
        &StageOptions::default(),
        &mut sink,
        &NeverCancel,
    ));
    assert_index_untouched(&repo, &before);
}

#[test]
fn stage_stops_on_cancel_argument() {
    let (_tmp, repo, _) = repo_with_modified_files();
    let before = index_bytes(&repo);
    let mut sink = CancelAfter::recorder();
    assert_cancelled(stage(
        &repo,
        &StageOptions::default(),
        &mut sink,
        &cancelled_flag(),
    ));
    assert_eq!(sink.seen, 0, "a cancelled stage hashes nothing");
    assert_index_untouched(&repo, &before);
    let outcome = stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage after cancel");
    assert_eq!(outcome.modified.len(), FILES);
}

#[test]
fn auto_stage_commit_reports_progress() {
    let (_tmp, repo, base) = repo_with_modified_files();
    let mut sink = CancelAfter::recorder();
    let outcome = commit_with_scope(
        &repo,
        &commit_req("change"),
        CommitScope::AutoStage,
        &mut sink,
        &NeverCancel,
    )
    .expect("commit");
    assert_eq!(outcome.parent, Some(base));
    assert!(sink.started.contains(&ProgressPhase::StagingFiles));
    assert_eq!(sink.units_of(ProgressPhase::StagingFiles), FILES as u64);
}

#[test]
fn auto_stage_commit_stops_when_the_sink_asks() {
    let (_tmp, repo, base) = repo_with_modified_files();
    let before = index_bytes(&repo);
    let mut sink = CancelAfter::new(ProgressPhase::StagingFiles, 10);
    assert_cancelled(commit_with_scope(
        &repo,
        &commit_req("change"),
        CommitScope::AutoStage,
        &mut sink,
        &NeverCancel,
    ));
    assert_eq!(head(&repo), base, "HEAD must not move");
    assert_index_untouched(&repo, &before);
}

#[test]
fn staged_and_path_commits_stop_on_cancel_argument() {
    let (_tmp, repo, base) = repo_with_modified_files();
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    let before = index_bytes(&repo);
    for scope in [
        CommitScope::Staged,
        CommitScope::Paths(grit_lib::worktree::pathspec::ResolvedPathspecs {
            specs: vec!["d".to_owned()],
            sources: vec!["d".to_owned()],
            scoped: true,
        }),
    ] {
        assert_cancelled(commit_with_scope(
            &repo,
            &commit_req("change"),
            scope,
            &mut NullProgress,
            &cancelled_flag(),
        ));
        assert_eq!(head(&repo), base);
        assert_index_untouched(&repo, &before);
    }
}

/// Stash push whose reset stops partway: the stored entry stays and the files the reset had
/// not reached keep their changes.
fn assert_stash_reset_cancelled(
    tmp: &tempfile::TempDir,
    repo: &Repository,
    opts: &StashPushOptions,
) {
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    assert_cancelled(repo.stash().push(opts, &mut sink, &NeverCancel));
    assert_eq!(
        repo.stash().list().expect("list").len(),
        1,
        "the stored entry stays"
    );
    let last = fs::read_to_string(tmp.path().join("d/f0299.txt")).expect("read");
    assert_eq!(last, "changed 299\n", "the reset stopped early");
    assert!(!repo.git_dir().join("index.lock").exists());
}

#[test]
fn stash_push_pathspec_reset_stops_when_the_sink_asks() {
    let (tmp, repo, _) = repo_with_modified_files();
    let opts = StashPushOptions {
        pathspecs: vec!["d".to_owned()],
        ..stash_opts()
    };
    assert_stash_reset_cancelled(&tmp, &repo, &opts);
}

#[test]
fn stash_push_staged_only_reset_stops_when_the_sink_asks() {
    let (tmp, repo, _) = repo_with_modified_files();
    stage(
        &repo,
        &StageOptions::default(),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("stage");
    let before = index_bytes(&repo);
    let opts = StashPushOptions {
        mode: StashMode::StagedOnly,
        ..stash_opts()
    };
    assert_stash_reset_cancelled(&tmp, &repo, &opts);
    assert_index_untouched(&repo, &before);
}

#[test]
fn stash_push_partial_reset_stops_when_the_sink_asks() {
    let (tmp, repo, _) = repo_with_modified_files();
    let ranges = (0..FILES)
        .map(|i| grit_lib::porcelain::hunk_select::LineRange {
            path: format!("d/f{i:04}.txt"),
            start: 1,
            end: 1,
        })
        .collect();
    let opts = StashPushOptions {
        selection: Some(grit_lib::porcelain::hunk_select::HunkSelection {
            ids: Vec::new(),
            ranges,
        }),
        ..stash_opts()
    };
    assert_stash_reset_cancelled(&tmp, &repo, &opts);
}

fn stash_opts() -> StashPushOptions {
    StashPushOptions {
        message: Some("wip".into()),
        include_untracked: false,
        identity: IDENT.parse().expect("signature"),
        selection: None,
        pathspecs: Vec::new(),
        dry_run: false,
        mode: StashMode::default(),
    }
}

#[test]
fn stash_push_stops_before_storing_the_entry() {
    let (tmp, repo, _) = repo_with_modified_files();
    let before = index_bytes(&repo);
    let mut sink = CancelAfter::new(ProgressPhase::StagingFiles, 10);
    assert_cancelled(repo.stash().push(&stash_opts(), &mut sink, &NeverCancel));
    assert!(repo.stash().list().expect("list").is_empty());
    assert_index_untouched(&repo, &before);
    let kept = fs::read_to_string(tmp.path().join("d/f0000.txt")).expect("read");
    assert_eq!(kept, "changed 0\n", "work tree keeps its changes");

    assert_cancelled(
        repo.stash()
            .push(&stash_opts(), &mut NullProgress, &cancelled_flag()),
    );
    assert!(repo.stash().list().expect("list").is_empty());
}

#[test]
fn stash_pop_cancel_keeps_the_entry() {
    let (tmp, repo, _) = repo_with_modified_files();
    let mut sink = CancelAfter::recorder();
    repo.stash()
        .push(&stash_opts(), &mut sink, &NeverCancel)
        .expect("push")
        .expect("stashed");
    assert!(sink.started.contains(&ProgressPhase::StagingFiles));
    assert!(sink.started.contains(&ProgressPhase::CheckingOutFiles));
    let reset = fs::read_to_string(tmp.path().join("d/f0000.txt")).expect("read");
    assert_eq!(reset, "base 0\n");

    let before = index_bytes(&repo);
    assert_cancelled(repo.stash().pop(
        0,
        &StashApplyOptions::default(),
        &mut NullProgress,
        &cancelled_flag(),
    ));
    assert_eq!(repo.stash().list().expect("list").len(), 1);
    assert_index_untouched(&repo, &before);
    let untouched = fs::read_to_string(tmp.path().join("d/f0000.txt")).expect("read");
    assert_eq!(
        untouched, "base 0\n",
        "a cancel before writing leaves files alone"
    );

    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    assert_cancelled(
        repo.stash()
            .pop(0, &StashApplyOptions::default(), &mut sink, &NeverCancel),
    );
    assert_eq!(repo.stash().list().expect("list").len(), 1);
    assert_index_untouched(&repo, &before);

    let rolled_back = fs::read_to_string(tmp.path().join("d/f0299.txt")).expect("read");
    assert_eq!(
        rolled_back, "base 299\n",
        "the cancelled checkout rolls back"
    );

    repo.stash()
        .pop(
            0,
            &StashApplyOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("pop");
    assert!(repo.stash().list().expect("list").is_empty());
}

/// `main` at a one-file commit, `side` one commit ahead adding [`FILES`] files.
fn repo_with_side_branch() -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
    let (tmp, repo) = init_repo();
    fs::write(tmp.path().join("base.txt"), "base\n").expect("write");
    let base = commit_all(&repo, "base");
    write_files(tmp.path(), "side", "side");
    let side = commit_all(&repo, "side");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/side", &side).expect("side");
    reset_to_commit(
        &repo,
        &ResetOptions::new(base, ResetMode::Hard, IDENT.parse().expect("sig"), "reset"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("reset");
    (tmp, repo, base, side)
}

#[test]
fn fast_forward_merge_stops_during_checkout() {
    let (_tmp, repo, base, side) = repo_with_side_branch();
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    assert_cancelled(integrate_branch(
        &repo,
        "refs/heads/main",
        base,
        side,
        "side",
        &mut sink,
        &NeverCancel,
    ));
    assert_eq!(branch(&repo, "main"), base, "the branch must not move");
    assert!(
        !repo
            .work_tree()
            .expect("wt")
            .join("side/f0299.txt")
            .exists(),
        "the cancelled checkout rolls back"
    );

    assert_cancelled(integrate_branch(
        &repo,
        "refs/heads/main",
        base,
        side,
        "side",
        &mut NullProgress,
        &cancelled_flag(),
    ));
    assert_eq!(branch(&repo, "main"), base);
}

#[test]
fn fast_forward_merge_completes_without_cancel() {
    let (tmp, repo, base, side) = repo_with_side_branch();
    let mut sink = CancelAfter::recorder();
    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        base,
        side,
        "side",
        &mut sink,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::FastForward { oid, .. } if oid == side));
    assert!(sink.started.contains(&ProgressPhase::CheckingOutFiles));
    assert!(tmp.path().join("side/f0000.txt").exists());
}

#[test]
fn conflicting_merge_cancel_restores_the_index() {
    let (tmp, repo) = init_repo();
    let root = tmp.path();
    fs::write(root.join("c.txt"), "base\n").expect("write");
    let base = commit_all(&repo, "base");
    fs::write(root.join("c.txt"), "side\n").expect("write");
    write_files(root, "side", "side");
    let side = commit_all(&repo, "side");
    reset_to_commit(
        &repo,
        &ResetOptions::new(base, ResetMode::Hard, IDENT.parse().expect("sig"), "reset"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("reset");
    fs::write(root.join("c.txt"), "main\n").expect("write");
    let main = commit_all(&repo, "main");
    let before = index_bytes(&repo);

    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    assert_cancelled(integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        side,
        "side",
        &mut sink,
        &NeverCancel,
    ));
    assert_eq!(branch(&repo, "main"), main);
    assert_index_untouched(&repo, &before);
    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    assert!(!root.join("side/f0299.txt").exists());
    assert_eq!(
        fs::read_to_string(root.join("c.txt")).expect("read"),
        "main\n"
    );

    let outcome = integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        side,
        "side",
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("merge");
    assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));
    assert!(repo.git_dir().join("MERGE_HEAD").exists());
}

#[test]
fn conflicting_merge_cancel_after_checkout_rolls_back_worktree() {
    let (tmp, repo) = init_repo();
    let root = tmp.path();
    fs::write(root.join("c.txt"), "base\n").expect("write");
    let base = commit_all(&repo, "base");
    fs::write(root.join("c.txt"), "side\n").expect("write");
    write_files(root, "side", "side");
    let side = commit_all(&repo, "side");
    reset_to_commit(
        &repo,
        &ResetOptions::new(base, ResetMode::Hard, IDENT.parse().expect("sig"), "reset"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("reset");
    fs::write(root.join("c.txt"), "main\n").expect("write");
    let main = commit_all(&repo, "main");
    let before = index_bytes(&repo);

    let mut sink = CancelAfter::cancel_when_phase_finishes(ProgressPhase::CheckingOutFiles);
    assert_cancelled(integrate_branch(
        &repo,
        "refs/heads/main",
        main,
        side,
        "side",
        &mut sink,
        &NeverCancel,
    ));
    assert_eq!(branch(&repo, "main"), main);
    assert_index_untouched(&repo, &before);
    assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    assert!(
        !root.join("side/f0299.txt").exists(),
        "post-checkout cancel must roll back checked-out files"
    );
    assert_eq!(
        fs::read_to_string(root.join("c.txt")).expect("read"),
        "main\n"
    );
}

fn rebase_start(upstream: ObjectId, tip: ObjectId) -> StartRebaseOptions {
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
            identity: IDENT.parse().expect("signature"),
            reflog_message: "rebase".into(),
            record_operation: None,
            hooks: grit_lib::PostOperationHooks::Skip,
        },
        pre_rebase: grit_lib::revwalk::history::PreRebase::Skip,
    }
}

/// `topic` (checked out) adds one file on `base`; `side` adds [`FILES`] files on `base`.
fn repo_for_rebase() -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
    let (tmp, repo, base, side) = repo_with_side_branch();
    grit_lib::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/topic").expect("HEAD");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/topic", &base).expect("topic");
    fs::write(tmp.path().join("topic.txt"), "topic\n").expect("write");
    let topic = commit_all(&repo, "topic");
    (tmp, repo, side, topic)
}

/// `topic` (checked out) and `main` both change `c.txt` on `base`.
fn repo_for_conflicting_rebase() -> (tempfile::TempDir, Repository, ObjectId, ObjectId) {
    let (tmp, repo) = init_repo();
    let root = tmp.path();
    fs::write(root.join("c.txt"), "base\n").expect("write");
    let base = commit_all(&repo, "base");
    fs::write(root.join("c.txt"), "main\n").expect("write");
    let main = commit_all(&repo, "main");
    grit_lib::refs::write_ref(repo.git_dir(), "refs/heads/topic", &base).expect("topic");
    grit_lib::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/topic").expect("HEAD");
    reset_to_commit(
        &repo,
        &ResetOptions::new(base, ResetMode::Hard, IDENT.parse().expect("sig"), "reset"),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect("reset");
    fs::write(root.join("c.txt"), "topic\n").expect("write");
    let topic = commit_all(&repo, "topic");
    (tmp, repo, main, topic)
}

#[test]
fn conflicting_rebase_cancelled_before_the_stop_changes_nothing() {
    let (tmp, repo, main, topic) = repo_for_conflicting_rebase();
    let err = start_rebase(
        &repo,
        &rebase_start(main, topic),
        &mut NullProgress,
        &cancelled_flag(),
    )
    .expect_err("cancelled");
    assert!(
        err.is_cancelled(),
        "expected a cancelled rebase, got {err:?}"
    );
    assert_eq!(branch(&repo, "topic"), topic);
    assert_eq!(head(&repo), topic);
    assert!(!repo.git_dir().join("rebase-merge").exists());
    assert_eq!(
        fs::read_to_string(tmp.path().join("c.txt")).expect("read"),
        "topic\n"
    );
}

#[test]
fn conflicting_rebase_stop_completes_once_published_and_continue_needs_resolution() {
    let (_tmp, repo, main, topic) = repo_for_conflicting_rebase();
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 0);
    let outcome = start_rebase(&repo, &rebase_start(main, topic), &mut sink, &NeverCancel)
        .expect("a published stop finishes installing");
    assert!(
        matches!(outcome, RebaseOutcome::Stopped(_)),
        "got {outcome:?}"
    );
    assert!(
        sink.flag.is_cancelled(),
        "the sink asked to stop during the stop checkout"
    );
    let index = repo.index_handle().load().expect("index");
    assert!(
        index.has_unmerged_entries(),
        "conflict stages are installed"
    );

    let err = continue_rebase(
        &repo,
        &rebase_start(main, topic),
        &mut NullProgress,
        &NeverCancel,
    )
    .expect_err("continue must require resolution");
    assert!(
        matches!(err, RebaseSessionError::UnmergedPaths),
        "got {err:?}"
    );
    assert_eq!(branch(&repo, "topic"), topic, "the branch has not moved");
}

#[test]
fn rebase_stops_on_cancel_argument_before_touching_anything() {
    let (_tmp, repo, side, topic) = repo_for_rebase();
    let err = start_rebase(
        &repo,
        &rebase_start(side, topic),
        &mut NullProgress,
        &cancelled_flag(),
    )
    .expect_err("cancelled");
    assert!(
        err.is_cancelled(),
        "expected a cancelled rebase, got {err:?}"
    );
    assert_eq!(branch(&repo, "topic"), topic);
    assert!(!repo.git_dir().join("rebase-merge").exists());
}

#[test]
fn rebase_cancelled_during_checkout_rolls_back() {
    let (tmp, repo, side, topic) = repo_for_rebase();
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    let err = start_rebase(&repo, &rebase_start(side, topic), &mut sink, &NeverCancel)
        .expect_err("cancelled");
    assert!(
        err.is_cancelled(),
        "expected a cancelled rebase, got {err:?}"
    );
    assert_eq!(branch(&repo, "topic"), topic, "refs roll back");
    assert!(
        !tmp.path().join("side/f0299.txt").exists(),
        "work tree rolls back"
    );

    let mut sink = CancelAfter::recorder();
    let outcome =
        start_rebase(&repo, &rebase_start(side, topic), &mut sink, &NeverCancel).expect("rebase");
    assert!(matches!(outcome, RebaseOutcome::Done { .. }));
    assert!(sink.started.contains(&ProgressPhase::CheckingOutFiles));
    assert!(tmp.path().join("side/f0299.txt").exists());
}

#[test]
fn pick_stops_before_moving_the_branch() {
    let (tmp, repo, side, topic) = repo_for_rebase();
    let req = ReplayRequest {
        commit: side,
        direction: ReplayDirection::Pick,
        committer: IDENT.parse().expect("signature"),
        message_override: None,
    };
    assert_cancelled(replay_commit(
        &repo,
        &req,
        &mut NullProgress,
        &cancelled_flag(),
    ));
    assert_eq!(branch(&repo, "topic"), topic);

    let before = index_bytes(&repo);
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    assert_cancelled(replay_commit(&repo, &req, &mut sink, &NeverCancel));
    assert_eq!(branch(&repo, "topic"), topic);
    assert!(
        !tmp.path().join("side/f0299.txt").exists(),
        "work tree rolls back"
    );
    assert_eq!(index_bytes(&repo), before);
}

#[test]
fn restore_stops_when_the_sink_asks() {
    let (tmp, repo, _) = repo_with_modified_files();
    let opts = RestoreOptions {
        pathspecs: vec![".".to_owned()],
        pathspec_sources: vec![".".to_owned()],
        source: RestoreSource::Index,
        staged: false,
        worktree: true,
        stage: None,
        remerge: None,
    };
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 5);
    let before_index = index_bytes(&repo);
    assert_cancelled(restore_paths(&repo, &opts, &mut sink, &NeverCancel));
    let last = fs::read_to_string(tmp.path().join("d/f0299.txt")).expect("read");
    assert_eq!(
        last, "changed 299\n",
        "cancelled restore must roll back the work tree"
    );
    assert_eq!(
        index_bytes(&repo),
        before_index,
        "index must stay unchanged"
    );

    restore_paths(&repo, &opts, &mut NullProgress, &NeverCancel).expect("restore");
    let last = fs::read_to_string(tmp.path().join("d/f0299.txt")).expect("read");
    assert_eq!(last, "base 299\n");
}

#[test]
fn restore_source_cancel_restores_directory_replacing_tracked_file() {
    let (tmp, repo) = init_repo();
    let wt = tmp.path();
    let victim = "0000-victim";
    fs::write(wt.join(victim), "old victim\n").expect("victim");
    write_files(wt, "bulk", "seed");
    commit_all(&repo, "old");
    fs::write(wt.join(victim), "new victim\n").expect("rewrite victim");
    write_files(wt, "bulk", "changed");
    commit_all(&repo, "new");

    fs::remove_file(wt.join(victim)).expect("rm victim file");
    fs::create_dir_all(wt.join(victim)).expect("mkdir victim");
    fs::write(wt.join(victim).join("sentinel.txt"), "keep me\n").expect("sentinel");

    let old_commit = grit_lib::revwalk::rev_parse::resolve_revision_as_commit_without_index_dwim(
        &repo, "HEAD~1",
    )
    .expect("old commit");
    let old_obj = repo.odb().read(&old_commit).expect("read commit");
    let old_tree = grit_lib::objects::parse_commit(&old_obj.data)
        .expect("parse")
        .tree;

    let opts = RestoreOptions {
        pathspecs: vec![".".to_owned()],
        pathspec_sources: vec![".".to_owned()],
        source: RestoreSource::Tree(old_tree),
        staged: false,
        worktree: true,
        stage: None,
        remerge: None,
    };
    let before_index = index_bytes(&repo);
    let mut sink = CancelAfter::new(ProgressPhase::CheckingOutFiles, 1);
    assert_cancelled(restore_paths(&repo, &opts, &mut sink, &NeverCancel));

    let victim_meta = fs::symlink_metadata(wt.join(victim)).expect("victim metadata");
    assert!(
        victim_meta.is_dir() && !victim_meta.file_type().is_symlink(),
        "victim must remain a directory after cancel"
    );
    assert!(
        wt.join(victim).join("sentinel.txt").is_file(),
        "untracked sentinel must survive cancel rollback"
    );
    assert_index_untouched(&repo, &before_index);
    assert!(
        !wt.join(victim).is_file(),
        "cancel must not leave a regular file at the victim path"
    );
}
