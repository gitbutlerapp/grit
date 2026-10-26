//! Typed failures for history rewrite planning.

use crate::error::Error;
use crate::objects::ObjectId;
use thiserror::Error;

/// Failure modes while building a [`super::RewritePlan`](crate::history::RewritePlan).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum HistoryError {
    /// Underlying object database, revision walk, or ref operation failed.
    #[error(transparent)]
    Library(Box<Error>),

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

    /// Object database, revwalk, or other repository read failed during planning.
    #[error("history rewrite planning failed: {message}")]
    Planning {
        /// Underlying failure summary.
        message: String,
    },

    /// `InsertBefore` was requested on a merge commit.
    #[error("insert-before on merge commit {commit} is not supported")]
    InsertBeforeOnMerge {
        /// Merge commit that cannot receive an insert-before edit.
        commit: ObjectId,
    },

    /// A three-way replay produced unmerged paths.
    #[error("replay of commit {commit} produced conflicts")]
    Conflict {
        /// Original commit that could not be replayed cleanly.
        commit: ObjectId,
        /// Repository-relative paths that conflicted, sorted and deduplicated.
        paths: Vec<String>,
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
    #[error("commit signing failed: {message}")]
    Signing {
        /// Underlying signing or config failure summary.
        message: String,
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
        /// Replay commit checked out when the command ran.
        commit: ObjectId,
        /// Successful command that changed the index or worktree.
        command: String,
        /// Changed paths relative to the replay commit, sorted and deduplicated.
        paths: Vec<String>,
        /// Captured invocations through the dirty command, in execution order.
        runs: Vec<super::ExecRunRecord>,
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
    #[error("ref update failed: {message}")]
    RefApply {
        /// Backend error summary.
        message: String,
    },

    /// Index or worktree update failed after refs were already moved.
    #[error("worktree update failed: {message}")]
    WorktreeApply {
        /// Underlying failure.
        message: String,
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

    /// Failed to read `.git/grit/history-last`.
    #[error("could not read history undo record: {message}")]
    UndoRecordRead {
        /// Parse or I/O failure summary.
        message: String,
    },

    /// Failed to write `.git/grit/history-last`.
    #[error("could not write history undo record: {message}")]
    UndoRecordWrite {
        /// I/O or lock failure summary.
        message: String,
    },

    /// Refs were updated but the worktree checkout did not finish (see `.git/grit/rewrite-apply/`).
    #[error("history rewrite checkout is incomplete")]
    RewriteApplyIncomplete,

    /// Checkout failed after refs were updated; rollback or worktree restore may also have failed.
    #[error("checkout failed after refs were updated")]
    ApplyRollbackIncomplete {
        /// Primary checkout failure.
        checkout: String,
        /// Ref rollback error, when rolling back failed.
        ref_rollback: Option<String>,
        /// Worktree/index restore error after checkout failed.
        worktree_restore: Option<String>,
    },
}

impl HistoryError {
    /// Whether the rewrite was cancelled ([`Error::Cancelled`]).
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        match self {
            Self::Library(e) => e.is_cancelled(),
            _ => false,
        }
    }
}

impl From<Error> for HistoryError {
    /// Unwraps [`Error::History`] back to the history failure it carries; every other library
    /// error becomes [`HistoryError::Library`].
    fn from(err: Error) -> Self {
        match err {
            Error::History(inner) => inner,
            other => Self::Library(Box::new(other)),
        }
    }
}
