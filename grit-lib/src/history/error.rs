//! Typed failures for history rewrite planning.

use crate::error::{Error, ErrorSummary};
use crate::objects::ObjectId;
use crate::operation::Conflict;
use crate::refs::store::RefStoreError;

use super::PlanningError;

/// Failure modes while building a [`super::RewritePlan`](crate::history::RewritePlan).
///
/// Surfaced on the umbrella as [`Error::History`](crate::error::Error::History); repository,
/// object, ref, and I/O failures during a rewrite are reported in their own areas.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum HistoryError {
    /// Invalid or inconsistent planning inputs (distinct from repository read failures, which
    /// surface as their own [`Error`](enum@crate::error::Error) area).
    #[error(transparent)]
    Planning(#[from] PlanningError),

    /// A resumable rebase session failed before or after invoking the rewrite engine.
    #[error(transparent)]
    RebaseSession(Box<super::RebaseSessionError>),

    /// The replacement commit message is empty after whitespace cleanup.
    #[error("commit message cannot be empty")]
    EmptyMessage,

    /// [`super::reword::reword_with`](crate::history::reword::reword_with) needs a message and/or author change.
    #[error("reword requires a new message or author")]
    NothingToReword,

    /// Author argument could not be parsed (`Name <email>` or bare email).
    #[error("{0}")]
    InvalidAuthor(#[from] crate::ident::AuthorArgError),

    /// [`super::fixup`](crate::history::fixup) requires a working tree and index.
    #[error("history fixup cannot run in a bare repository")]
    BareRepository,

    /// No staged changes relative to `HEAD`'s tree.
    #[error("nothing to fixup: no staged changes")]
    NothingStaged,

    /// Hunk id or line-range selection failed before planning changed refs.
    #[error(transparent)]
    HunkSelect(#[from] crate::porcelain::hunk_select::HunkSelectError),

    /// Generating a change id for a replacement commit failed (for example the
    /// [`ChangeIdGenerator`](crate::change_id::ChangeIdGenerator) had no entropy).
    #[error(transparent)]
    ChangeId(#[from] crate::change_id::ChangeIdError),

    /// Applying staged changes onto the target would make that commit empty.
    #[error("fixup makes commit {commit} empty")]
    FixupMakesTargetEmpty {
        /// Target commit that would become empty.
        commit: ObjectId,
    },

    /// A pathspec split selected no changes from the target commit.
    #[error("pathspec did not select any changes from the commit")]
    SplitSelectsNothing,

    /// A pathspec split selected every change in the target commit.
    #[error("pathspec selected every change in the commit")]
    SplitSelectsEverything,

    /// `InsertBefore` was requested on a merge commit.
    #[error("insert-before on merge commit {commit} is not supported")]
    InsertBeforeOnMerge {
        /// Merge commit that cannot receive an insert-before edit.
        commit: ObjectId,
    },

    /// A three-way replay produced unmerged paths. Nothing was written to refs.
    #[error("replay of commit {commit} produced conflicts")]
    Conflict {
        /// Original commit that could not be replayed cleanly.
        commit: ObjectId,
        /// First line of that commit's message.
        subject: String,
        /// Conflicted paths and how they conflicted, sorted by path.
        conflicts: Box<Vec<Conflict>>,
        /// Full names of the scoped branches whose replay failed, sorted, when `commit` is not
        /// on `HEAD`'s history: under [`RefScope::Branches`](super::RefScope::Branches) another
        /// branch built on the edited commits could not be moved onto the new history, and
        /// [`RefScope::Head`](super::RefScope::Head) would rewrite only the current branch.
        /// Empty when the conflict is on the checked-out history itself.
        refs: Vec<String>,
    },

    /// A merge commit cannot be dropped, reordered, or otherwise edited in an unsupported way.
    #[error("merge commit {commit} cannot be dropped or reordered in this rewrite")]
    MergeCommitInReplay {
        /// Merge commit that cannot be replayed.
        commit: ObjectId,
    },

    /// A root commit was dropped or otherwise needs a missing parent.
    #[error("root commit {commit} cannot be dropped or replayed without a parent")]
    RootCommit {
        /// Root commit involved in the error.
        commit: ObjectId,
    },

    /// Empty-policy [`super::EmptyPolicy::Abort`](crate::history::EmptyPolicy::Abort) tripped.
    #[error("commit {commit} became empty after replay")]
    BecameEmpty {
        /// Commit that would become empty.
        commit: ObjectId,
    },

    /// An edited commit is not on the history reachable from the active scope.
    #[error("commit {commit} is not reachable from HEAD for this rewrite scope")]
    NotInHistory {
        /// Commit named in the edit map but outside scope.
        commit: ObjectId,
    },

    /// A move anchor is not on the first-parent chain of `HEAD`.
    #[error("commit {commit} is not on the linear first-parent history of HEAD")]
    MoveTargetNotInHistory {
        /// Anchor commit that is missing from the chain.
        commit: ObjectId,
    },

    /// The move would leave commit order unchanged.
    #[error("commit {commit} is already at the requested position")]
    NoOp {
        /// Commit that would not move.
        commit: ObjectId,
    },

    /// Commits are not on the same first-parent chain or a merge sits in the segment.
    #[error("history move requires a linear first-parent range without merge commits")]
    NotLinear,

    /// A scoped branch tip is not on the ancestry path being rewritten.
    #[error("branch {name} diverged before the rewritten commits and cannot be updated")]
    DivergentBranch {
        /// Short or full branch name (`refs/heads/...`).
        name: String,
    },

    /// Another linked worktree has this branch checked out.
    #[error("branch {name} is checked out in worktree {worktree}")]
    BranchCheckedOutElsewhere {
        /// Branch ref short or full name.
        name: String,
        /// Path to the blocking worktree.
        worktree: String,
    },

    /// Commit signing failed while writing a replacement commit object.
    #[error("commit signing failed")]
    Signing {
        /// Signing program invoked (`gpg`, `ssh-keygen`, …).
        program: String,
        /// Subprocess exit status when the program ran.
        status: Option<i32>,
        /// Subprocess stderr (or a short summary when spawn failed).
        stderr: Box<String>,
    },

    /// Applying the plan would overwrite staged or unstaged worktree changes on affected paths.
    #[error("local changes would be overwritten")]
    LocalChangesWouldBeOverwritten {
        /// Repository-relative paths that block the checkout.
        paths: Vec<String>,
    },

    /// The index or worktree differs from `HEAD` before `rebase --exec` runs.
    #[error("you have uncommitted changes — commit them before rebasing with --exec")]
    UncommittedChanges,

    /// A successful exec changed tracked paths. Edits and the replay HEAD are preserved;
    /// original branch refs are unchanged and rebase recovery state is persisted.
    #[error("rebase --exec left tracked changes at commit {commit}; save the edits before continuing, or abort the rebase")]
    ExecDirty {
        /// Detached `HEAD` after the command: the replay commit, or the commit the command
        /// amended it into. Recovery expects `HEAD` here.
        commit: ObjectId,
        /// Successful command that changed the index or worktree.
        command: String,
        /// Changed paths relative to `commit`, sorted and deduplicated.
        paths: Vec<String>,
        /// Captured invocations through the dirty command, in execution order.
        runs: Vec<super::ExecRunRecord>,
    },

    /// Replaying a later commit onto one that an `--exec` command amended or extended
    /// produced conflicts. The conflicted replay is checked out with conflict markers on
    /// top of the rewritten commit, branch refs are unchanged, and recovery state is
    /// persisted: resolve and stage, then [`super::continue_rebase`]; or
    /// [`super::skip_rebase`] to drop the commit, or [`super::abort_rebase`].
    #[error(
        "replay of commit {commit} onto the commit rewritten by rebase --exec produced conflicts"
    )]
    ExecConflict {
        /// Original commit that could not be replayed cleanly.
        commit: ObjectId,
        /// Conflicted paths and how they conflicted, sorted by path.
        conflicts: Box<Vec<Conflict>>,
        /// Captured invocations before the conflict, in execution order.
        runs: Vec<super::ExecRunRecord>,
    },

    /// A successful `--exec` command left `HEAD` somewhere other than the replay commit, an
    /// amended replacement of it, or new commits on top of either (for example it switched
    /// branches or reset away the commit). Repository state was restored; commits the
    /// command created remain in the object database.
    #[error("rebase --exec command moved HEAD away from commit {commit}; it may only amend or add commits on top of it")]
    ExecHeadMoved {
        /// Replay commit checked out when the command ran.
        commit: ObjectId,
        /// Commit `HEAD` resolved to after the command, when readable.
        head: Option<ObjectId>,
    },

    /// A `rebase --exec` command exited non-zero; repository state was restored.
    #[error("rebase --exec command failed at commit {commit} (exit {exit_code})")]
    ExecFailed {
        /// Replacement commit that was checked out when the command failed.
        commit: ObjectId,
        /// Subprocess exit code.
        exit_code: i32,
        /// Subprocess stdout from the failing command.
        stdout: Vec<u8>,
        /// Subprocess stderr from the failing command.
        stderr: Vec<u8>,
        /// Successful `--exec` runs before the failure (stdout/stderr captured).
        completed_runs: Vec<super::ExecRunRecord>,
    },

    /// Selected commits do not form one connected component with allowed parents.
    #[error("the selected commits are not a connected revision range")]
    RangeNotConnected,

    /// More than one tip commit appears in the selected range.
    #[error("the revision range contains more than one tip commit")]
    MultipleTips,

    /// A fixup/amend/squash commit targets a commit outside the range.
    #[error("cannot squash {commit}: its fixup/amend/squash target is outside the range")]
    MarkerTargetOutsideRange {
        /// Marker commit that could not be folded.
        commit: ObjectId,
    },

    /// A `squash!` commit would lose its message without combining interactively.
    #[error("squashing {commit} would discard its message; use --combine or -m")]
    SquashDiscardsMessage {
        /// Commit whose message would be dropped.
        commit: ObjectId,
    },

    /// A planned ref no longer matches the stored value (compare-and-swap failure).
    #[error("ref {name} changed since the rewrite plan was built")]
    RefCasMismatch {
        /// Ref that failed the expected-old check.
        name: String,
    },

    /// `HEAD` no longer points at the ref or commit recorded in the plan.
    #[error("HEAD moved since the rewrite plan was built")]
    HeadMovedSincePlan {
        /// Ref `HEAD` pointed at when planning, if symbolic.
        planned_ref: Option<String>,
        /// Commit `HEAD` pointed at when planning.
        planned_commit: ObjectId,
    },

    /// Ref storage rejected the update batch.
    #[error(transparent)]
    RefApply(#[from] RefStoreError),

    /// Index or worktree update failed after refs were already moved.
    #[error("worktree update failed: {source}")]
    WorktreeApply {
        /// Underlying failure after ref updates.
        #[source]
        source: ErrorSummary,
    },

    /// The worktree update after refs moved could not proceed (missing snapshot data, a path
    /// that could not be restored, or no work tree).
    #[error("worktree update failed: {detail}")]
    WorktreeApplyFailed {
        /// What could not be done.
        detail: String,
    },

    /// No `.git/grit/history-last` record exists.
    #[error("no history rewrite to undo")]
    NoUndoRecord,

    /// A ref or `HEAD` moved since the recorded rewrite.
    #[error("ref {name} changed since the last history rewrite — undo is no longer safe")]
    UndoStale {
        /// Ref that no longer matches the recorded post-rewrite tip.
        name: String,
    },

    /// Failed to read `.git/grit/history-last` or rewrite journal files.
    #[error("could not read history undo record: {source}")]
    UndoRecordRead {
        /// Underlying I/O failure.
        #[source]
        source: ErrorSummary,
    },

    /// `.git/grit/history-last` or a rewrite journal file is malformed.
    #[error("history undo record is malformed: {detail}")]
    UndoRecordCorrupt {
        /// What is wrong with the record.
        detail: String,
    },

    /// Failed to write `.git/grit/history-last` or rewrite journal files.
    #[error("could not write history undo record: {source}")]
    UndoRecordWrite {
        /// Underlying I/O or lock failure.
        #[source]
        source: ErrorSummary,
    },

    /// A history undo record or rewrite journal could not be written because its contents
    /// are invalid (for example a snapshot path that is not repository-relative).
    #[error("could not write history undo record: {detail}")]
    UndoRecordUnwritable {
        /// What is wrong with the contents.
        detail: String,
    },

    /// Refs were updated but the worktree checkout did not finish (see `.git/grit/rewrite-apply/`).
    #[error("history rewrite checkout is incomplete")]
    RewriteApplyIncomplete,

    /// Checkout failed after refs were updated; rollback or worktree restore may also have failed.
    #[error("checkout failed after refs were updated: {checkout}")]
    ApplyRollbackIncomplete {
        /// Primary checkout failure.
        #[source]
        checkout: ErrorSummary,
        /// Ref rollback error, when rolling back failed.
        ref_rollback: Option<ErrorSummary>,
        /// Worktree/index restore error after checkout failed.
        worktree_restore: Option<ErrorSummary>,
    },
}

/// [`HistoryError::WorktreeApply`] for an I/O failure.
#[must_use]
pub(crate) fn worktree_apply_io(err: std::io::Error) -> Error {
    worktree_apply_engine(Error::Io(err))
}

/// [`HistoryError::WorktreeApply`] for a library failure.
#[must_use]
pub(crate) fn worktree_apply_engine(err: impl Into<Error>) -> Error {
    HistoryError::WorktreeApply {
        source: ErrorSummary::from(err.into()),
    }
    .into()
}

/// [`HistoryError::WorktreeApplyFailed`] with `detail`.
#[must_use]
pub(crate) fn worktree_apply_str(detail: impl Into<String>) -> Error {
    HistoryError::WorktreeApplyFailed {
        detail: detail.into(),
    }
    .into()
}

#[must_use]
pub(crate) fn undo_record_write_engine(err: impl Into<Error>) -> Error {
    HistoryError::UndoRecordWrite {
        source: ErrorSummary::from(err.into()),
    }
    .into()
}

#[must_use]
pub(crate) fn undo_record_read_io(err: std::io::Error) -> Error {
    HistoryError::UndoRecordRead {
        source: ErrorSummary::from(Error::Io(err)),
    }
    .into()
}

#[must_use]
pub(crate) fn undo_record_write_io(err: std::io::Error) -> Error {
    undo_record_write_engine(Error::Io(err))
}

#[must_use]
pub(crate) fn undo_record_read_str(detail: impl Into<String>) -> Error {
    HistoryError::UndoRecordCorrupt {
        detail: detail.into(),
    }
    .into()
}

#[must_use]
pub(crate) fn undo_record_write_str(detail: impl Into<String>) -> Error {
    HistoryError::UndoRecordUnwritable {
        detail: detail.into(),
    }
    .into()
}

impl HistoryError {
    /// Whether the rewrite was cancelled (a cancellation recorded as the cause of a failed
    /// worktree update, undo-record access, or rollback).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::WorktreeApply { source }
            | Self::UndoRecordRead { source }
            | Self::UndoRecordWrite { source } => source.is_cancelled(),
            Self::ApplyRollbackIncomplete {
                checkout,
                ref_rollback,
                worktree_restore,
            } => {
                checkout.is_cancelled()
                    || ref_rollback.as_ref().is_some_and(|e| e.is_cancelled())
                    || worktree_restore.as_ref().is_some_and(|e| e.is_cancelled())
            }
            _ => false,
        }
    }
}
