//! Data types for describing history edits and rewrite plans.

use std::collections::BTreeMap;

use crate::objects::ObjectId;

/// Description of a new commit to insert immediately before an existing one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewCommit {
    /// Root tree for the inserted commit.
    pub tree: ObjectId,
    /// Full author ident (name, email, timestamp, tz).
    pub author: String,
    /// Commit message body (trailing newline optional).
    pub message: String,
}

/// Per-commit edit applied during planning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitEdit {
    /// Replace fields on the commit, then replay descendants.
    Rewrite {
        /// Optional replacement tree OID.
        tree: Option<ObjectId>,
        /// Optional replacement message.
        message: Option<String>,
        /// Optional replacement author ident line.
        author: Option<String>,
        /// Optional replacement parent list (mapped OIDs should be used by callers when chaining).
        parents: Option<Vec<ObjectId>>,
    },
    /// Omit the commit; descendants replay onto its mapped first parent.
    Drop,
    /// Insert `commit` before the mapped original, which becomes the insert's child.
    InsertBefore(NewCommit),
}

/// Map from original commit OID to the edit applied at that commit.
pub type HistoryEdits = BTreeMap<ObjectId, CommitEdit>;

/// Which local refs may move when a plan is applied later.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum RefScope {
    /// Every local branch whose tip descends from the oldest edited commit, plus `HEAD`.
    #[default]
    Branches,
    /// Only the ref `HEAD` currently points at (symbolic or detached).
    Head,
}

/// Behavior when a replayed commit's tree matches its new parent's tree but did not before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EmptyPolicy {
    /// Skip creating a commit; map to the new parent OID.
    #[default]
    Drop,
    /// Keep an empty commit with the merged tree.
    Keep,
    /// Fail planning with [`super::HistoryError::BecameEmpty`](crate::history::HistoryError::BecameEmpty).
    Abort,
}

/// Inputs for [`super::plan_rewrite`](crate::history::plan_rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewriteOptions {
    /// Which refs to consider when collecting tips and ref updates.
    pub scope: RefScope,
    /// How to handle commits that become empty during replay.
    pub empty: EmptyPolicy,
    /// Committer ident for every newly written commit (must include explicit time; no hidden clock).
    pub committer: String,
}

/// One original commit and its replacement (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewrittenCommit {
    /// Commit OID before rewrite.
    pub old: ObjectId,
    /// Replacement OID, or `None` when the commit was dropped from history.
    pub new: Option<ObjectId>,
}

/// Planned movement of one ref (refs, index, and worktree are not updated by [`super::plan_rewrite`](crate::history::plan_rewrite)).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedRefUpdate {
    /// Full ref name (for example `refs/heads/main`).
    pub name: String,
    /// Previous tip OID.
    pub old: ObjectId,
    /// New tip OID after rewrite.
    pub new: ObjectId,
}

/// Effect on `HEAD` from the rewrite (commit pointer and trees).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HeadEffect {
    /// Whether `HEAD`'s commit OID changes.
    pub commit_changes: bool,
    /// Tree OID at `HEAD` before rewrite, when known.
    pub old_tree: Option<ObjectId>,
    /// Tree OID at `HEAD` after rewrite, when known.
    pub new_tree: Option<ObjectId>,
}

/// Result of [`super::plan_rewrite`](crate::history::plan_rewrite): new objects are already in the ODB.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewritePlan {
    /// Every original commit in rewrite order and its replacement.
    pub commits: Vec<RewrittenCommit>,
    /// Ref tip updates implied by the rewrite (not applied by planning).
    pub ref_updates: Vec<PlannedRefUpdate>,
    /// How `HEAD` would change if the plan were applied.
    pub head: HeadEffect,
}
