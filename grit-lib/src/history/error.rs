//! Typed failures for history rewrite planning.

use crate::objects::ObjectId;
use thiserror::Error;

/// Failure modes while building a [`super::RewritePlan`](crate::history::RewritePlan).
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum HistoryError {
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
}
