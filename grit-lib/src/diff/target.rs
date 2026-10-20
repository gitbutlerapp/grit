//! Which trees, commits, and worktree sides a repository diff compares.

use crate::objects::ObjectId;

/// What to compare when asking whether a diff has any real changes.
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffTarget {
    /// Uncommitted changes: index and working tree together against `HEAD`'s tree.
    Uncommitted,
    /// Staged changes only: index against `HEAD`'s tree.
    Staged,
    /// Unstaged changes only: working tree against the index.
    Unstaged,
    /// Working tree against a specific tree (typically another ref's tree).
    WorktreeAgainst {
        /// Tree oid to compare the worktree to.
        tree: ObjectId,
    },
    /// The patch introduced by a single commit (first parent tree vs commit tree).
    Commit {
        /// Commit whose introduced change is tested.
        commit: ObjectId,
    },
    /// Tree diff between two commits (after rename detection policy in [`super::DiffOptions`]).
    Commits {
        /// Older side (merge base or range start).
        base: ObjectId,
        /// Newer side (range end).
        tip: ObjectId,
    },
}
