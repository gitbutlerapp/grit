//! Typed failures for history rewrite planning.

use crate::objects::ObjectId;
use thiserror::Error;

/// Failure modes while building a [`super::RewritePlan`](crate::history::RewritePlan).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HistoryError {
    /// The replacement commit message is empty after whitespace cleanup.
    #[error("commit message cannot be empty")]
    EmptyMessage,

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
        /// Repository-relative paths that conflicted.
        paths: Vec<String>,
    },

    /// A merge commit would have to be replayed onto a new parent.
    #[error("replay of merge commit {commit} is not supported")]
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

    /// Applying the plan would overwrite staged or unstaged worktree changes on affected paths.
    #[error("local changes would be overwritten")]
    LocalChangesWouldBeOverwritten {
        /// Repository-relative paths that block the checkout.
        paths: Vec<String>,
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
}
