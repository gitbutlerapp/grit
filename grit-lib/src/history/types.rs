//! Data types for describing history edits and rewrite plans.

use std::collections::BTreeMap;

use crate::change_id::ChangeId;
use crate::ident::Signature;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
pub enum RefScope {
    /// Every local branch whose tip descends from the oldest edited commit, plus `HEAD`.
    #[default]
    Branches,
    /// Only the ref `HEAD` currently points at (symbolic or detached).
    Head,
}

/// Behavior when a replayed commit's tree matches its new parent's tree but did not before.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RewriteOptions {
    /// Which refs to consider when collecting tips and ref updates.
    pub scope: RefScope,
    /// How to handle commits that become empty during replay.
    pub empty: EmptyPolicy,
    /// Committer for every newly written commit (for example from
    /// [`Repository::identity`](crate::repo::Repository::identity); no hidden clock).
    #[serde(with = "signature_header_line")]
    pub committer: Signature,
    /// When `true`, replayed commits keep the source committer line instead of [`Self::committer`].
    pub preserve_committer: bool,
    /// When `true`, honor [`commit.gpgsign`](crate::config::ConfigSet) and sign replacement commits
    /// (same path as [`crate::porcelain::commit::prepare_commit_bytes`]). When `false`, commits are
    /// written unsigned and external signers are not invoked (for example history `--dry-run`).
    pub sign: bool,
}

impl RewriteOptions {
    /// The validated header line for [`Self::committer`], checked before any object is written.
    pub(crate) fn committer_line(&self) -> Result<String, super::HistoryError> {
        self.committer.validate().map_err(|err| {
            super::HistoryError::Library(Box::new(crate::error::Error::InvalidSignature(err)))
        })?;
        Ok(self.committer.header_line())
    }
}

/// One original commit and its replacement (if any).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RewrittenCommit {
    /// Commit OID before rewrite.
    pub old: ObjectId,
    /// Replacement OID, or `None` when the commit was dropped from history.
    pub new: Option<ObjectId>,
    /// `change-id` header on the replacement commit, or `None` when dropped or absent.
    pub change_id: Option<ChangeId>,
    /// First line of the replacement commit message, or the original when dropped.
    pub subject: Option<String>,
    /// `true` when [`Self::new`] is a commit that already existed (a patch already applied
    /// upstream, or a fast-forward onto the new base) rather than a replacement the rewrite
    /// wrote. Such rows are not reported to `post-rewrite`.
    pub reused: bool,
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
    /// Commit a rebase replays onto. Git checks it out before replaying, so `post-checkout`
    /// names it as the new `HEAD`; `None` for history edits, where `post-checkout` names the
    /// rewritten tip instead.
    pub checkout_onto: Option<ObjectId>,
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
    /// `(old, new)` for every commit the rewrite replaced, in rewrite order.
    ///
    /// Dropped commits, [reused](RewrittenCommit::reused) commits and commits whose replacement
    /// is the original object are left out; this is the list `post-rewrite` receives on stdin.
    #[must_use]
    pub fn rewritten_pairs(&self) -> Vec<(ObjectId, ObjectId)> {
        self.commits
            .iter()
            .filter(|c| !c.reused)
            .filter_map(|c| c.new.filter(|new| *new != c.old).map(|new| (c.old, new)))
            .collect()
    }

    /// Subject line for each row in [`Self::commits`], in the same order.
    ///
    /// # Errors
    ///
    /// Returns [`super::HistoryError`] when a subject must be loaded from the object database and
    /// the commit is missing or corrupt.
    pub fn subjects(
        &self,
        repo: &crate::repo::Repository,
    ) -> Result<Vec<Option<String>>, super::HistoryError> {
        self.commits
            .iter()
            .map(|row| super::subject::rewritten_row_subject(repo, row))
            .collect()
    }

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
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ApplyOptions {
    /// Committer identity for reflog entries (timestamp is taken from the signature).
    #[serde(with = "signature_header_line")]
    pub identity: Signature,
    /// Reflog message for every ref update in the batch.
    pub reflog_message: String,
    /// When set, persist ref movements to `.git/grit/history-last` after a successful apply.
    pub record_operation: Option<String>,
    /// Whether to run `post-checkout` (when `HEAD`'s tree changes) and `post-rewrite rebase`
    /// after the rewrite is applied.
    #[serde(default)]
    pub hooks: crate::hooks::PostOperationHooks,
}

/// Result of [`super::undo::undo_last_rewrite`](crate::history::undo::undo_last_rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UndoOutcome {
    /// Ref updates that were applied or would be applied (`--dry-run`).
    pub ref_updates: Vec<PlannedRefUpdate>,
    /// Commit OID `HEAD` resolves to after undo (or would resolve to when dry-run).
    pub head: ObjectId,
    /// Previous and new `HEAD` commit OIDs when undo moves `HEAD`'s commit.
    pub head_change: Option<(ObjectId, ObjectId)>,
    /// Whether refs were left unchanged.
    pub dry_run: bool,
}

/// Result of [`super::apply_rewrite`](crate::history::apply_rewrite).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRewrite {
    /// Ref updates that were committed.
    pub ref_updates: Vec<PlannedRefUpdate>,
    /// Commit OID `HEAD` resolves to after apply.
    pub head: ObjectId,
    /// Post-operation hooks that ran ([`crate::hooks::HookName::PostCheckout`],
    /// [`crate::hooks::HookName::PostRewrite`]).
    pub hooks_run: Vec<crate::hooks::HookName>,
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
                    change_id: None,
                    subject: None,
                    reused: false,
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
    fn rewritten_pairs_skip_dropped_unchanged_and_reused_rows() {
        let mut plan = make_plan(
            &[
                (1, Some(11)),
                (2, None),
                (3, Some(3)),
                (4, Some(14)),
                (5, Some(15)),
            ],
            &[],
            None,
            false,
        );
        plan.commits[4].reused = true;
        assert_eq!(
            plan.rewritten_pairs(),
            vec![(oid(1), oid(11)), (oid(4), oid(14))]
        );
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

/// Persist a [`Signature`] as its header line, the same text earlier plan files stored.
mod signature_header_line {
    use serde::{Deserialize, Deserializer, Serializer};

    use crate::ident::Signature;

    pub(super) fn serialize<S: Serializer>(
        signature: &Signature,
        serializer: S,
    ) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&signature.header_line())
    }

    pub(super) fn deserialize<'de, D: Deserializer<'de>>(
        deserializer: D,
    ) -> Result<Signature, D::Error> {
        let line = String::deserialize(deserializer)?;
        line.parse().map_err(serde::de::Error::custom)
    }
}
