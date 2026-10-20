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
    /// Insert `insert` before the commit; the mapped original keeps its tree and uses the insert
    /// as its parent, optionally with a new message.
    SplitBefore {
        /// New parent commit to insert.
        insert: NewCommit,
        /// When set, replaces the remaining commit's message.
        rest_message: Option<String>,
    },
    /// Replay this commit onto `parent` instead of its mapped first parent (rebase range root).
    Reparent {
        /// Replacement first parent for the replay.
        parent: ObjectId,
    },
    /// Map to an existing commit without writing a new object (patch already applied upstream).
    Reuse(ObjectId),
}

/// Pathspec strings passed to history helpers (same syntax as `git` pathspecs on the CLI).
pub type Pathspec<'a> = &'a [String];

/// Options for [`super::split`](crate::history::split).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SplitOptions {
    /// When set, replaces the remaining commit's message after the split.
    pub rest_message: Option<String>,
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
    /// When `true`, replayed commits keep the source committer line instead of [`Self::committer`].
    pub preserve_committer: bool,
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
    /// Commit OID `HEAD` resolved to when the plan was built.
    pub old_commit: Option<ObjectId>,
    /// Full ref name when `HEAD` was symbolic at plan time (`refs/heads/...`); `None` when detached.
    pub head_ref: Option<String>,
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

impl RewritePlan {
    /// Tip OID after rewrite when a ref update moves `old_tip`, if any.
    #[must_use]
    pub fn mapped_tip(&self, old_tip: ObjectId) -> Option<ObjectId> {
        self.ref_updates
            .iter()
            .find(|u| u.old == old_tip)
            .map(|u| u.new)
    }

    /// Replacement OID for the original commit `old`.
    ///
    /// Returns `None` when `old` was dropped from history or is not part of the plan.
    #[must_use]
    pub fn replacement(&self, old: ObjectId) -> Option<ObjectId> {
        self.commits
            .iter()
            .find(|c| c.old == old)
            .and_then(|c| c.new)
    }

    /// `HEAD`'s commit before and after the rewrite, as `(old, new)`.
    ///
    /// The new commit is the tip of the ref update that moves `HEAD`'s old commit, else the
    /// commit's replacement, else the old commit itself. Returns `None` when
    /// [`HeadEffect::commit_changes`] is `false` or `HEAD` was unborn at plan time.
    #[must_use]
    pub fn head_change(&self) -> Option<(ObjectId, ObjectId)> {
        if !self.head.commit_changes {
            return None;
        }
        let old = self.head.old_commit?;
        let new = self
            .mapped_tip(old)
            .or_else(|| self.replacement(old))
            .unwrap_or(old);
        Some((old, new))
    }

    /// `update <ref> <new> <old>` lines accepted by `git update-ref --stdin`.
    #[must_use]
    pub fn ref_update_lines(&self) -> Vec<String> {
        self.ref_updates
            .iter()
            .map(|u| format!("update {} {} {}", u.name, u.new.to_hex(), u.old.to_hex()))
            .collect()
    }
}

/// Identity and reflog text recorded when [`super::apply_rewrite`](crate::history::apply_rewrite) moves refs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOptions {
    /// Reflog identity (`Name <email>`).
    pub identity: String,
    /// Reflog message for every ref update in the batch.
    pub reflog_message: String,
}

/// Result of [`super::apply_rewrite`](crate::history::apply_rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRewrite {
    /// Ref updates that were committed.
    pub ref_updates: Vec<PlannedRefUpdate>,
    /// Commit OID `HEAD` resolves to after apply.
    pub head: ObjectId,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    fn make_plan(
        commits: &[(u8, Option<u8>)],
        ref_updates: &[(u8, u8)],
        head: Option<u8>,
        commit_changes: bool,
    ) -> RewritePlan {
        RewritePlan {
            commits: commits
                .iter()
                .map(|&(old, new)| RewrittenCommit {
                    old: oid(old),
                    new: new.map(oid),
                })
                .collect(),
            ref_updates: ref_updates
                .iter()
                .map(|&(old, new)| PlannedRefUpdate {
                    name: "refs/heads/main".into(),
                    old: oid(old),
                    new: oid(new),
                })
                .collect(),
            head: HeadEffect {
                commit_changes,
                old_commit: head.map(oid),
                ..HeadEffect::default()
            },
        }
    }

    #[test]
    fn replacement_finds_rewritten_commits_only() {
        let plan = make_plan(&[(1, Some(11)), (2, None)], &[], None, false);
        assert_eq!(plan.replacement(oid(1)), Some(oid(11)));
        assert_eq!(plan.replacement(oid(2)), None);
        assert_eq!(plan.replacement(oid(3)), None);
    }

    #[test]
    fn head_change_is_none_when_head_commit_stays() {
        let plan = make_plan(&[(1, Some(11))], &[(1, 11)], Some(1), false);
        assert_eq!(plan.head_change(), None);
        let unborn = make_plan(&[(1, Some(11))], &[], None, true);
        assert_eq!(unborn.head_change(), None);
    }

    #[test]
    fn head_change_prefers_ref_update_tip() {
        let plan = make_plan(&[(1, Some(11)), (2, Some(12))], &[(2, 13)], Some(2), true);
        assert_eq!(plan.head_change(), Some((oid(2), oid(13))));
    }

    #[test]
    fn head_change_falls_back_to_replacement_then_old() {
        let detached = make_plan(&[(1, Some(11))], &[], Some(1), true);
        assert_eq!(detached.head_change(), Some((oid(1), oid(11))));
        let dropped = make_plan(&[(1, None)], &[], Some(1), true);
        assert_eq!(dropped.head_change(), Some((oid(1), oid(1))));
    }
}
