//! [`History`]: history edits and rebase scoped to one [`Repository`].

use crate::command_runner::CommandRunner;
use crate::ident::Signature;
use crate::objects::ObjectId;
use crate::porcelain::hunk_select::HunkSelection;
use crate::progress::{CancelToken, ProgressSink};
use crate::repo::Repository;

use super::absorb::{AbsorbOptions, AbsorbPlan};
use super::apply::RewriteApplyRecovery;
use super::exec::{ExecReport, ExecSpec};
use super::fixup::FixupOptions;
use super::last::HistoryLastRecord;
use super::r#move::Placement;
use super::rebase::{RebaseCheckPlan, RebaseOntoOptions, RebasePlan, RebaseRequest};
use super::rebase_session::{RebaseAbort, RebaseOutcome, StartRebaseOptions};
use super::reword::RewordOptions;
use super::squash::SquashOptions;
use super::types::{
    AppliedRewrite, ApplyOptions, HistoryEdits, Pathspec, RewriteOptions, RewritePlan,
    RewrittenCommit, SplitOptions, UndoMode, UndoOutcome,
};

/// History edits (reword, drop, move, fixup, squash, split, absorb), undo and rebase for one
/// open repository, from [`Repository::history`].
///
/// Edits are two steps. A planning method (for example [`Self::reword`] or [`Self::plan_rewrite`])
/// writes the replacement commits into the object database and returns a [`RewritePlan`]; refs,
/// the index and the work tree are left alone. [`Self::apply`] (or [`Self::apply_fixup`] /
/// [`Self::apply_absorb`] for edits that consume staged changes) then moves every ref in one
/// transaction and updates the work tree. Replacement commits keep the source's `change-id`
/// header, so a rewritten change keeps its id (see [`crate::change_id`]).
///
/// Rebase has the same split: [`Self::plan_rebase`] predicts, and [`Self::start_rebase`] runs a
/// resumable session that stops on conflicts, continued with [`Self::continue_rebase`],
/// [`Self::skip_rebase`] or [`Self::abort_rebase`].
///
/// # Examples
///
/// ```rust
/// use grit_lib::progress::{CancelToken, NullProgress};
/// use grit_lib::repo::Repository;
/// use grit_lib::revwalk::history::{
///     ApplyOptions, EmptyPolicy, RefScope, RewordOptions, RewriteOptions,
/// };
///
/// # fn example(repo: &Repository, commit: grit_lib::objects::ObjectId) -> Result<(), Box<dyn std::error::Error>> {
/// let identity: grit_lib::ident::Signature = "Rewriter <r@example.com> 1700000000 +0000".parse()?;
/// let rewrite = RewriteOptions::new(identity.clone())
///     .with_scope(RefScope::Branches)
///     .with_empty(EmptyPolicy::Drop)
///     .with_sign(false);
/// let history = repo.history();
/// let plan = history.reword(
///     commit,
///     &RewordOptions::default().with_message("Better subject\n"),
///     &rewrite,
/// )?;
/// let apply = ApplyOptions::new(identity, "history reword")
///     .with_record_operation(Some("reword".into()));
/// history.apply(&plan, &apply, &mut NullProgress, &CancelToken::never())?;
/// # Ok(())
/// # }
/// ```
#[derive(Debug, Copy, Clone)]
pub struct History<'repo> {
    repo: &'repo Repository,
}

impl<'repo> History<'repo> {
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self { repo }
    }

    /// Borrow the underlying repository.
    #[must_use]
    pub fn repository(&self) -> &'repo Repository {
        self.repo
    }

    /// Build a rewrite plan: walk affected history, write replacement commits to the ODB, and
    /// record ref movements without touching refs, the index, or the worktree.
    ///
    /// Replacement commits preserve `change-id`, legacy `gitbutler-change-id`, and other extra
    /// headers from the template (signatures are stripped). When
    /// [`ChangeIdPolicy`](crate::change_id::ChangeIdPolicy) is enabled and the source commit has
    /// no stored change id, the rewritten commit stores the Jujutsu-derived id for the original
    /// commit oid. New commits inserted by the plan (for example split parents) receive fresh ids
    /// from [`ChangeIdPolicy::id_for_new_commit`](crate::change_id::ChangeIdPolicy::id_for_new_commit).
    ///
    /// When [`RewriteOptions::sign`] is `true`, replacement commits honor `commit.gpgsign`. When
    /// `sign` is `false`, commits are written unsigned and external signers are not invoked.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`](super::HistoryError) for conflicts, unsupported merges, scope violations, empty-policy
    /// aborts, [`HistoryError::Planning`](super::HistoryError::Planning) for invalid planning inputs, and the object, revision
    /// or ref area errors for read failures.
    pub fn plan_rewrite(
        &self,
        edits: &HistoryEdits,
        opts: &RewriteOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::plan::plan_rewrite(self.repo, edits, opts)
    }

    /// Run the work-tree safety checks of [`Self::apply`] without moving refs.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::LocalChangesWouldBeOverwritten`](super::HistoryError::LocalChangesWouldBeOverwritten) or
    /// [`HistoryError::WorktreeApply`](super::HistoryError::WorktreeApply).
    pub fn verify_plan(&self, plan: &RewritePlan) -> crate::error::Result<()> {
        super::apply::verify_rewrite_plan(self.repo, plan)
    }

    /// Apply `plan`: move every planned ref in one transaction, then check out `HEAD`'s new tree.
    ///
    /// Work-tree updates report [`crate::progress::ProgressPhase::CheckingOutFiles`] to
    /// `progress`. A cancel or a failure after refs moved rolls the refs and work tree back.
    /// When [`ApplyOptions::record_operation`] is set, the ref movements are recorded for
    /// [`Self::undo_last`].
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::LocalChangesWouldBeOverwritten`](super::HistoryError::LocalChangesWouldBeOverwritten),
    /// [`HistoryError::HeadMovedSincePlan`](super::HistoryError::HeadMovedSincePlan), [`HistoryError::RefCasMismatch`](super::HistoryError::RefCasMismatch),
    /// [`HistoryError::RefApply`](super::HistoryError::RefApply), or [`HistoryError::WorktreeApply`](super::HistoryError::WorktreeApply).
    pub fn apply(
        &self,
        plan: &RewritePlan,
        opts: &ApplyOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<AppliedRewrite> {
        super::apply::apply_rewrite(self.repo, plan, opts, progress, cancel)
    }

    /// Plan removal of `commit` from history and replay of its descendants onto its parent.
    ///
    /// Writes replacement commit objects into the ODB; refs are updated only when the caller
    /// applies the returned [`RewritePlan`].
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::RootCommit`](super::HistoryError::RootCommit) when `commit` has no parents,
    /// [`HistoryError::MergeCommitInReplay`](super::HistoryError::MergeCommitInReplay) when `commit` is a merge, and an object error when
    /// `commit` cannot be read as a commit (for example
    /// [`ObjectError::UnexpectedKind`](crate::error::ObjectError::UnexpectedKind)).
    /// Other failures come from [`Self::plan_rewrite`].
    pub fn drop_commit(
        &self,
        commit: ObjectId,
        opts: &RewriteOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::drop::drop(self.repo, commit, opts)
    }

    /// Plan moving `commit` to `placement` on the linear first-parent history of `HEAD`.
    ///
    /// Writes replacement commit objects into the ODB; refs are updated only when the caller
    /// applies the returned [`RewritePlan`].
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::NotInHistory`](super::HistoryError::NotInHistory), [`HistoryError::MoveTargetNotInHistory`](super::HistoryError::MoveTargetNotInHistory),
    /// [`HistoryError::NoOp`](super::HistoryError::NoOp), [`HistoryError::NotLinear`](super::HistoryError::NotLinear), or errors from replay planning.
    /// An unborn or invalid `HEAD` is [`RefError::UnbornHead`](crate::error::RefError::UnbornHead)
    /// or [`RefError::InvalidHead`](crate::error::RefError::InvalidHead).
    pub fn move_commit(
        &self,
        commit: ObjectId,
        placement: Placement,
        opts: &RewriteOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::r#move::move_commit(self.repo, commit, placement, opts)
    }

    /// Plan a message and/or author rewrite of `commit` and replay of its descendants.
    ///
    /// Writes replacement commit objects into the ODB; refs are updated only when the caller
    /// applies the returned [`RewritePlan`].
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::NothingToReword`](super::HistoryError::NothingToReword) when both [`RewordOptions::message`] and
    /// [`RewordOptions::author`] are absent, [`HistoryError::EmptyMessage`](super::HistoryError::EmptyMessage) when a provided
    /// message is empty after cleanup, [`HistoryError::InvalidAuthor`](super::HistoryError::InvalidAuthor) when the author string is
    /// invalid, or any error from [`Self::plan_rewrite`].
    pub fn reword(
        &self,
        commit: ObjectId,
        options: &RewordOptions,
        opts: &RewriteOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::reword::reword(self.repo, commit, options, opts)
    }

    /// Plan a pathspec-limited split of `commit`: matching changes move into a new parent commit
    /// whose message is `split_message`.
    ///
    /// The target commit keeps its tree (and message unless [`SplitOptions::rest_message`] is
    /// set). Descendants replay with parent-only updates when their trees are unchanged.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::SplitSelectsNothing`](super::HistoryError::SplitSelectsNothing), [`HistoryError::SplitSelectsEverything`](super::HistoryError::SplitSelectsEverything),
    /// [`HistoryError::InsertBeforeOnMerge`](super::HistoryError::InsertBeforeOnMerge), [`HistoryError::EmptyMessage`](super::HistoryError::EmptyMessage), or errors from
    /// [`Self::plan_rewrite`].
    pub fn split(
        &self,
        commit: ObjectId,
        pathspec: Pathspec<'_>,
        split_message: String,
        opts: &SplitOptions,
        rewrite_opts: &RewriteOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::split::split(
            self.repo,
            commit,
            pathspec,
            split_message,
            opts,
            rewrite_opts,
        )
    }

    /// Plan squashing `range` into one commit and replaying descendants.
    ///
    /// `range` uses the same revision syntax as `git rev-list` (for example `base..tip`,
    /// `^excl tip`).
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::Planning`](super::HistoryError::Planning) when `range` is empty, is not a `<base>..<tip>` range,
    /// or selects fewer than two commits; the revision, object or ref area error when a revision
    /// does not resolve or a read fails; and other [`HistoryError`](super::HistoryError)
    /// variants when markers refuse or planning fails.
    pub fn squash(
        &self,
        range: &[&str],
        opts: &SquashOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::squash::squash(self.repo, range, opts)
    }

    /// Plan a fixup of `target`: merge staged index changes onto `target`'s tree and replay
    /// descendants.
    ///
    /// When [`FixupOptions::selection`] is set, only the selected unstaged hunks are folded; the
    /// on-disk index is unchanged until [`Self::apply_fixup_hunks`]. Otherwise apply the plan
    /// with [`Self::apply_fixup`] and the paths from [`Self::staged_paths`].
    ///
    /// Writes new commit objects into the ODB; refs are updated only when the caller applies the
    /// plan.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::BareRepository`](super::HistoryError::BareRepository), [`HistoryError::NothingStaged`](super::HistoryError::NothingStaged), merge
    /// [`HistoryError::Conflict`](super::HistoryError::Conflict), empty-policy failures, and other planning errors.
    pub fn fixup(
        &self,
        target: ObjectId,
        opts: &FixupOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::fixup::fixup(self.repo, target, opts)
    }

    /// Repository-relative paths that differ between the index and `HEAD`'s tree (stage 0):
    /// the staged paths a fixup or absorb consumes when applied.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::BareRepository`](super::HistoryError::BareRepository) or planning failures when the index or `HEAD`
    /// cannot be read.
    pub fn staged_paths(&self) -> crate::error::Result<Vec<String>> {
        super::fixup::staged_fixup_paths(self.repo)
    }

    /// Repository-relative paths touched by a hunk `selection` limited to `pathspecs`.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::HunkSelect`](super::HistoryError::HunkSelect) when ids or line ranges cannot be resolved.
    pub fn fixup_selection_paths(
        &self,
        selection: &HunkSelection,
        pathspecs: &[String],
    ) -> crate::error::Result<Vec<String>> {
        super::fixup::fixup_selection_paths(self.repo, selection, pathspecs)
    }

    /// Apply a fixup [`RewritePlan`] and align the index with the new `HEAD` on `staged_paths`
    /// (normally [`Self::staged_paths`] read before planning).
    ///
    /// Stops when `cancel` is set and rolls back partial work.
    ///
    /// # Errors
    ///
    /// Same as [`Self::apply`], plus index update failures after a successful ref move.
    pub fn apply_fixup(
        &self,
        plan: &RewritePlan,
        opts: &ApplyOptions,
        staged_paths: &[String],
        cancel: &CancelToken,
    ) -> crate::error::Result<AppliedRewrite> {
        super::apply::apply_fixup_rewrite(self.repo, plan, opts, staged_paths, cancel)
    }

    /// Apply a fixup plan built with [`FixupOptions::selection`]: stage the selected hunks, then
    /// apply like [`Self::apply_fixup`].
    ///
    /// Pre-existing staged edits on the same path as a selected unstaged hunk are preserved in
    /// the index and worktree after apply. On failure or cancel, the index is restored to its
    /// pre-call state.
    ///
    /// # Errors
    ///
    /// Same as [`Self::apply_fixup`], plus hunk resolution and staging failures.
    pub fn apply_fixup_hunks(
        &self,
        plan: &RewritePlan,
        opts: &ApplyOptions,
        selection: &HunkSelection,
        pathspecs: &[String],
        cancel: &CancelToken,
    ) -> crate::error::Result<AppliedRewrite> {
        super::fixup::apply_fixup_hunk_selection(
            self.repo, plan, opts, selection, pathspecs, cancel,
        )
    }

    /// Plan absorption of staged hunks into commits between [`AbsorbOptions::base`] and `HEAD`.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::BareRepository`](super::HistoryError::BareRepository), [`HistoryError::NothingStaged`](super::HistoryError::NothingStaged), merge
    /// [`HistoryError::Conflict`](super::HistoryError::Conflict), and other planning errors.
    pub fn absorb(&self, opts: &AbsorbOptions) -> crate::error::Result<AbsorbPlan> {
        super::absorb::absorb(self.repo, opts)
    }

    /// Default exclusive base for [`Self::absorb`] when [`AbsorbOptions::base`] is `None`.
    ///
    /// # Errors
    ///
    /// Returns planning failures when `HEAD` or branch metadata cannot be read.
    pub fn default_absorb_base(&self, head: ObjectId) -> crate::error::Result<ObjectId> {
        super::absorb::default_absorb_base(self.repo, head)
    }

    /// Apply an absorb [`AbsorbPlan`] and leave unassigned hunks staged.
    ///
    /// Stops when `cancel` is set and rolls back partial work.
    ///
    /// # Errors
    ///
    /// Same as [`Self::apply_fixup`].
    pub fn apply_absorb(
        &self,
        absorb: &AbsorbPlan,
        opts: &ApplyOptions,
        cancel: &CancelToken,
    ) -> crate::error::Result<AppliedRewrite> {
        super::apply::apply_absorb_rewrite(self.repo, absorb, opts, cancel)
    }

    /// Subject for a [`RewrittenCommit`] row (uses the stored subject when present).
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError`](super::HistoryError) when a commit cannot be read from the object database.
    pub fn rewritten_row_subject(
        &self,
        row: &RewrittenCommit,
    ) -> crate::error::Result<Option<String>> {
        super::subject::rewritten_row_subject(self.repo, row)
    }

    /// The record of the last applied rewrite (`.git/grit/history-last`), when present.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::UndoRecordCorrupt`](super::HistoryError::UndoRecordCorrupt) when the file exists but is invalid.
    pub fn last(&self) -> crate::error::Result<Option<HistoryLastRecord>> {
        super::last::read_history_last(self.repo)
    }

    /// Revert the last recorded history rewrite when refs are still at the post-rewrite tips.
    ///
    /// Builds the inverse ref plan from [`Self::last`], verifies each ref is still at its
    /// recorded `new` OID ([`HistoryError::UndoStale`](super::HistoryError::UndoStale)), then (for [`UndoMode::Apply`]) applies
    /// ref updates and the matching worktree transition. On success, writes the inverse as the
    /// new record so a second call redoes the original rewrite. Like `reset`, undo runs no
    /// post-operation hooks, whatever [`ApplyOptions::hooks`] says. [`UndoMode::DryRun`] runs
    /// the checks and reports what would change without touching anything.
    ///
    /// Fixup and absorb fold staged (or selected unstaged) edits into history. Undoing them puts
    /// those edits back: every consumed path gets the index entry and work tree file it had
    /// before the command ([`UndoOutcome::restored_paths`]), and a second undo returns to the
    /// post-command state. Records written before grit kept that state still move refs;
    /// [`UndoOutcome::unrestored_edits`] then names the commit that holds the edits.
    ///
    /// Refs and `HEAD` are read and moved through the repository's ref store
    /// ([`Repository::ref_store`]); the undo record itself is a file under the git directory.
    /// Stops when `cancel` is set and rolls back partial work.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::NoUndoRecord`](super::HistoryError::NoUndoRecord), [`HistoryError::UndoStale`](super::HistoryError::UndoStale),
    /// [`HistoryError::LocalChangesWouldBeOverwritten`](super::HistoryError::LocalChangesWouldBeOverwritten) (including a restored path edited since
    /// the command), and other apply failures.
    pub fn undo_last(
        &self,
        opts: &ApplyOptions,
        mode: UndoMode,
        cancel: &CancelToken,
    ) -> crate::error::Result<UndoOutcome> {
        super::undo::undo_last_rewrite(self.repo, opts, mode, cancel)
    }

    /// Whether `.git/grit/rewrite-apply/` records an incomplete rewrite apply (an apply that was
    /// killed between moving refs and finishing the work tree).
    #[must_use]
    pub fn apply_journal_active(&self) -> bool {
        super::apply::rewrite_apply_journal_active(self.repo.git_dir())
    }

    /// Finish or roll back an interrupted rewrite apply recorded in the journal.
    ///
    /// Recovery holds `index.lock` throughout. A lock left by a process that is no longer
    /// running (per its `core.lockfilePid` sidecar) is removed first; any other existing lock
    /// means the apply may still be in flight, so nothing is touched.
    ///
    /// When every planned ref already points at its new OID and the journal comes from
    /// [`Self::apply`], the captured pre-apply index and worktree paths are restored and the
    /// checkout is run again, so a half-written checkout is not mistaken for local edits. If that
    /// checkout still fails, refs are rolled back instead. When every ref still matches the
    /// pre-apply OID, the journal is cleared. Otherwise refs are rolled back and the snapshots
    /// restored.
    ///
    /// A journal directory without its `meta` file is left over from a kill while the snapshots
    /// were being written, before any ref moved; it is removed once `index.lock` can be taken.
    ///
    /// # Returns
    ///
    /// `None` when no journal was pending (or it recorded no ref movement), otherwise what
    /// recovery did.
    ///
    /// # Errors
    ///
    /// Returns [`Error::LockHeld`](crate::error::Error::LockHeld) when another process holds
    /// `index.lock`, [`HistoryError::UndoRecordRead`](super::HistoryError::UndoRecordRead) when the journal is present
    /// but invalid,
    /// and apply failures when rollback cannot complete.
    pub fn recover_interrupted_apply(&self) -> crate::error::Result<Option<RewriteApplyRecovery>> {
        super::apply::recover_interrupted_rewrite_apply(self.repo)
    }

    /// Abort an incomplete rewrite apply by rolling refs back and restoring snapshots.
    ///
    /// Runs under `index.lock`, like [`Self::recover_interrupted_apply`].
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::RewriteApplyIncomplete`](super::HistoryError::RewriteApplyIncomplete) when no journal is pending,
    /// [`Error::LockHeld`](crate::error::Error::LockHeld) when another process holds
    /// `index.lock`, [`HistoryError::UndoRecordRead`](super::HistoryError::UndoRecordRead) when the journal is invalid,
    /// and apply
    /// failures when rollback cannot complete.
    pub fn abort_interrupted_apply(&self) -> crate::error::Result<RewriteApplyRecovery> {
        super::apply::abort_interrupted_rewrite_apply(self.repo)
    }

    /// Try to finish or roll back a pending rewrite-apply journal, then fail when it remains.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::RewriteApplyIncomplete`](super::HistoryError::RewriteApplyIncomplete) when recovery could not clear the
    /// journal, and the errors of [`Self::recover_interrupted_apply`].
    pub fn ensure_apply_ready(&self) -> crate::error::Result<()> {
        super::apply::ensure_rewrite_apply_ready(self.repo)
    }

    /// Build a [`RebaseRequest`] for pull/rebase and implicit-upstream rebase after a fetch.
    ///
    /// When `use_fork_point` is true, the exclude boundary comes from the reflog of
    /// `upstream_reflog_ref` (the upstream tracking ref) with Git's octopus fast-forward guard.
    /// Otherwise the merge base between `head` and `upstream_tip` is used. A `landed` boundary
    /// (the upstream tip before the fetch) is composed when it is a descendant of the fork
    /// boundary.
    ///
    /// # Errors
    ///
    /// Propagates merge-base and reflog errors.
    pub fn rebase_request_after_fetch(
        &self,
        head: ObjectId,
        upstream_tip: ObjectId,
        upstream_reflog_ref: &str,
        landed: Option<ObjectId>,
        use_fork_point: bool,
    ) -> crate::error::Result<RebaseRequest> {
        super::rebase::rebase_request_after_fetch(
            self.repo,
            head,
            upstream_tip,
            upstream_reflog_ref,
            landed,
            use_fork_point,
        )
    }

    /// Build a [`RebaseRequest`] for `grit rebase` / `grit history rebase`.
    ///
    /// When `explicit_onto` is set, `upstream_oid` and `onto_oid` are taken literally. Otherwise
    /// fork-point rules match Git: enabled for the implicit `@{upstream}` (`upstream_spec` is
    /// `None`) unless `rebase.forkPoint=false`, disabled for an explicit upstream unless
    /// `use_fork_point` is `Some(true)`.
    ///
    /// # Errors
    ///
    /// Propagates revision, config, and planning errors.
    pub fn rebase_run_request(
        &self,
        branch_tip: ObjectId,
        upstream_oid: ObjectId,
        onto_oid: Option<ObjectId>,
        upstream_spec: Option<&str>,
        explicit_onto: bool,
        use_fork_point: Option<bool>,
    ) -> crate::error::Result<RebaseRequest> {
        super::rebase::rebase_run_request(
            self.repo,
            branch_tip,
            upstream_oid,
            onto_oid,
            upstream_spec,
            explicit_onto,
            use_fork_point,
        )
    }

    /// Plan a rebase with fork-point detection and bounded cherry patch-id lookup.
    ///
    /// The replayed range is `merge-base(upstream, branch_tip)..branch_tip`, applied onto
    /// `onto.unwrap_or(upstream)`, matching `git rebase [--onto X] U` on `branch_tip`.
    ///
    /// [`RebaseOntoOptions::pre_rebase`] runs once the branch is known not to be up to date and
    /// before any commit is replayed (Git's order), so a refusal happens before a replay
    /// conflict is found or a new object is written.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::Planning`](super::HistoryError::Planning) when tips or upstream are inconsistent,
    /// [`Error::Hook`](crate::error::Error::Hook) when `pre-rebase` refuses the rebase or cannot
    /// start, and other planning failures except conflicts (those
    /// are returned as [`RebasePlan::Conflict`]).
    pub fn plan_rebase(
        &self,
        request: &RebaseRequest,
        opts: &RebaseOntoOptions,
    ) -> crate::error::Result<RebasePlan> {
        super::rebase::plan_rebase(self.repo, request, opts)
    }

    /// Predict whether a rebase would succeed without updating refs, the index, or the worktree.
    ///
    /// New commit objects may still be written to the object database (as for
    /// [`Self::plan_rebase`]). [`RebaseOntoOptions::pre_rebase`] is not run: nothing is about to
    /// be rewritten.
    ///
    /// # Errors
    ///
    /// Same planning errors as [`Self::plan_rebase`], except conflicts are returned as
    /// [`RebaseCheckPlan::Conflict`].
    pub fn plan_rebase_check(
        &self,
        request: &RebaseRequest,
        opts: &RebaseOntoOptions,
    ) -> crate::error::Result<RebaseCheckPlan> {
        super::rebase::plan_rebase_check(self.repo, request, opts)
    }

    /// Plan replay of `upstream..tips` onto `onto` in topological order.
    ///
    /// The `upstream` argument is the exclude boundary (fork point), not necessarily the user's
    /// upstream ref; [`Self::plan_rebase`] computes the fork from upstream and tip instead.
    /// [`RebaseOntoOptions::pre_rebase`] is not run.
    ///
    /// Merge commits in the range are replayed with updated parents and trees, unless
    /// [`RebaseOntoOptions::flatten_leaf_merges`] linearizes the replay. Under
    /// [`RefScope::Branches`](super::RefScope::Branches), every local branch whose tip descends
    /// from the rewritten range is moved to its replayed tip, except other branches at the same
    /// commit as `HEAD`, which keep the pre-rebase history.
    ///
    /// # Errors
    ///
    /// Returns [`HistoryError::Conflict`](super::HistoryError::Conflict) and other planning failures from
    /// [`Self::plan_rewrite`].
    pub fn rebase_onto(
        &self,
        upstream: ObjectId,
        onto: ObjectId,
        tips: &[ObjectId],
        opts: &RebaseOntoOptions,
    ) -> crate::error::Result<RewritePlan> {
        super::rebase::rebase_onto(self.repo, upstream, onto, tips, opts)
    }

    /// Run `--exec` commands at every commit of `plan` before any ref moves, amending the plan
    /// with commits the commands create.
    ///
    /// Commands run through `runner` in a detached checkout of each commit. Use this to verify a
    /// rebase plan; [`Self::start_rebase`] with [`StartRebaseOptions::exec`] does it as part of a
    /// session.
    ///
    /// # Errors
    ///
    /// - [`HistoryError::ExecFailed`](super::HistoryError::ExecFailed) when a command exits non-zero.
    /// - [`HistoryError::ExecDirty`](super::HistoryError::ExecDirty) when a command leaves local changes; the stop is saved so
    ///   [`Self::continue_rebase`] can resume.
    /// - [`HistoryError::ExecConflict`](super::HistoryError::ExecConflict) when replaying a later commit onto a rewritten one
    ///   conflicts. The conflict is checked out and exec recovery state is saved; resolve and
    ///   [`Self::continue_rebase`], or [`Self::skip_rebase`] / [`Self::abort_rebase`].
    /// - [`HistoryError::Conflict`](super::HistoryError::Conflict) when such a replay of a merge commit conflicts, and
    ///   [`HistoryError::BecameEmpty`](super::HistoryError::BecameEmpty) when a replay becomes empty under
    ///   [`super::EmptyPolicy::Abort`] (state is rolled back).
    /// - I/O, checkout, or subprocess failures.
    pub fn verify_rebase(
        &self,
        plan: &mut RewritePlan,
        spec: &ExecSpec,
        rewrite: &RewriteOptions,
        runner: &dyn CommandRunner,
    ) -> crate::error::Result<ExecReport> {
        super::exec::verify_rebase_with_exec(self.repo, plan, spec, rewrite, runner)
    }

    /// Start a resumable rebase of `opts.request.branch_tip` onto the effective onto commit.
    ///
    /// Unless the branch is already up to date, runs `pre-rebase` before any commit is replayed
    /// (see [`RebaseOntoOptions::pre_rebase`]); a refusal returns
    /// [`Error::Hook`](crate::error::Error::Hook), and nothing is changed or written.
    ///
    /// On a clean replay, applies the plan atomically and returns [`RebaseOutcome::Done`]. On
    /// conflict, writes Git-compatible rebase state and returns [`RebaseOutcome::Stopped`]
    /// without moving the branch ref away from its pre-rebase tip. Both record the pre-rebase
    /// tip as `ORIG_HEAD`, which finishing or aborting the rebase leaves in place; an up-to-date
    /// branch leaves `ORIG_HEAD` untouched.
    ///
    /// With [`StartRebaseOptions::exec`] set, its commands run through `runner` at each replayed
    /// commit before any ref moves (`grit rebase --exec`, see [`Self::verify_rebase`]); when
    /// they all succeed, the (possibly amended) plan is applied atomically and the result is
    /// [`RebaseOutcome::ExecDone`]. Commits in the rebase range run the commands even when the
    /// branch is already based on the upstream (matching `git rebase -x`). A replay conflict
    /// before any command runs stops as without exec; [`Self::continue_rebase`] runs the
    /// commands at the resolved commit. Without exec, `runner` is not used.
    ///
    /// Work-tree updates report [`crate::progress::ProgressPhase::CheckingOutFiles`] to
    /// `progress`. `cancel` is checked after planning and during the apply and stop checkouts; a
    /// cancel returns
    /// [`Error::Cancelled`](crate::error::Error::Cancelled) (see
    /// [`Error::is_cancelled`](crate::error::Error::is_cancelled)). The last cancel point is before a conflict stop is
    /// recorded, so a cancel leaves nothing changed; once the stop state and `HEAD` are written,
    /// its checkout runs to completion and the rebase stops with its conflicts as without a
    /// cancel.
    ///
    /// # Errors
    ///
    /// Returns [`RebaseSessionError::RebaseAlreadyInProgress`](super::RebaseSessionError::RebaseAlreadyInProgress) when grit or `git` already has a
    /// rebase in progress, [`RebaseSessionError::NotOnBranch`](super::RebaseSessionError::NotOnBranch) when `HEAD` is unborn, and
    /// planning/apply failures otherwise. When the replay would stop on conflicts but the index
    /// or worktree has local changes (or an untracked file sits where the stop checks out a
    /// path), returns
    /// [`WorktreeError::DirtyWorktree`](crate::worktree::WorktreeError::DirtyWorktree) or the overwrite error and
    /// leaves the repository untouched, because stopping would discard that work. With exec,
    /// also the errors of [`Self::verify_rebase`]; the two that save a resumable stop
    /// ([`HistoryError::ExecDirty`](super::HistoryError::ExecDirty) and [`HistoryError::ExecConflict`](super::HistoryError::ExecConflict)) record `ORIG_HEAD`
    /// first, the others roll back and leave it alone.
    pub fn start_rebase(
        &self,
        opts: &StartRebaseOptions,
        runner: &dyn CommandRunner,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<RebaseOutcome> {
        match &opts.exec {
            None => super::rebase_session::start_rebase(self.repo, opts, progress, cancel),
            Some(spec) => super::rebase_session::start_rebase_with_exec(
                self.repo, opts, spec, runner, progress, cancel,
            ),
        }
    }

    /// Finish the current conflicted commit and replay the remaining commits.
    ///
    /// For an exec stop, requires a clean index/worktree at the recorded replay `HEAD`, then
    /// resumes at the next command (the successful dirty command is not rerun). Save exec edits
    /// with a stash before continuing. A moved `HEAD` is rejected; save new commits on a branch,
    /// abort, and restart in that case. For an exec replay conflict
    /// ([`HistoryError::ExecConflict`](super::HistoryError::ExecConflict)), commits the staged resolution and runs the commands at
    /// it. Later replays and new commits use the rewrite options saved when the exec stop was
    /// recorded, not `opts.rebase.rewrite`. Exec commands run through the repository's
    /// [`Repository::command_runner`].
    ///
    /// # Errors
    ///
    /// Returns [`RebaseSessionError::UnmergedPaths`](super::RebaseSessionError::UnmergedPaths) when the index still records conflicts,
    /// [`RebaseSessionError::ForeignRebaseInProgress`](super::RebaseSessionError::ForeignRebaseInProgress) when `git` started the rebase,
    /// [`RebaseSessionError::NoRebaseInProgress`](super::RebaseSessionError::NoRebaseInProgress) when no rebase is in progress, and, for an
    /// exec stop, [`RebaseSessionError::StaleExecPlan`](super::RebaseSessionError::StaleExecPlan) (changing nothing) when the rebased
    /// branch or a ref the plan updates was moved during the stop. Cancellation follows
    /// [`Self::start_rebase`]: a cancel before the replayed commits are applied keeps the rebase
    /// stopped where it was.
    pub fn continue_rebase(
        &self,
        opts: &StartRebaseOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<RebaseOutcome> {
        super::rebase_session::continue_rebase(self.repo, opts, progress, cancel)
    }

    /// Skip the current todo commit and replay the rest.
    ///
    /// For an exec stop, requires the edits to be saved or removed first, then skips remaining
    /// commands at that replay commit; the already replayed commit is kept. For an exec replay
    /// conflict, drops the conflicted commit and runs that commit's commands at the current
    /// `HEAD`, as Git's todo list would.
    ///
    /// # Errors
    ///
    /// Returns [`RebaseSessionError::ForeignRebaseInProgress`](super::RebaseSessionError::ForeignRebaseInProgress) when `git` started the rebase,
    /// [`RebaseSessionError::NoRebaseInProgress`](super::RebaseSessionError::NoRebaseInProgress) when no rebase is in progress,
    /// [`RebaseSessionError::StaleExecPlan`](super::RebaseSessionError::StaleExecPlan) as for [`Self::continue_rebase`], and
    /// planning/apply failures otherwise. Cancellation follows [`Self::continue_rebase`].
    pub fn skip_rebase(
        &self,
        opts: &StartRebaseOptions,
        progress: &mut dyn ProgressSink,
        cancel: &CancelToken,
    ) -> crate::error::Result<RebaseOutcome> {
        super::rebase_session::skip_rebase(self.repo, opts, progress, cancel)
    }

    /// Cancel the in-progress rebase and restore the branch to `orig-head`.
    ///
    /// Also cancels a rebase that `git` started (`rebase-merge/` or `rebase-apply/` without
    /// grit's plan): `HEAD` is re-attached to the rebased branch (or detached, when the rebase
    /// started detached), the index and worktree are reset to `orig-head`, and the state
    /// directory is removed. Either way, unresolved rerere records for the abandoned conflict are
    /// cleared (`MERGE_RR`), as `git rebase --abort` does.
    ///
    /// The branch is restored to `orig-head` even when something else (another worktree, a
    /// `git branch -f`, an amend on the checked-out branch) moved it while the rebase was
    /// stopped; such movements are listed in [`RebaseAbort::overwritten`] and logged as
    /// `rebase (abort)` in the branch reflog with `identity`. `identity` also signs the reflog
    /// entries of the reset.
    ///
    /// # Errors
    ///
    /// Returns [`RebaseSessionError::NoRebaseInProgress`](super::RebaseSessionError::NoRebaseInProgress) when no rebase is in progress,
    /// [`RebaseSessionError::ForeignRebaseInProgress`](super::RebaseSessionError::ForeignRebaseInProgress) when a git-started rebase recorded an
    /// autostash or has no readable `orig-head` (finish those with `git rebase --abort`), and
    /// [`RebaseSessionError::AbortRefMoved`](super::RebaseSessionError::AbortRefMoved) when, for an exec stop, the branch moves again
    /// between reading and resetting it (nothing is restored and the abort can be retried).
    pub fn abort_rebase(&self, identity: &Signature) -> crate::error::Result<RebaseAbort> {
        super::rebase_session::abort_rebase(self.repo, identity)
    }
}
