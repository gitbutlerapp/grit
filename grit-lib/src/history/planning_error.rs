//! Typed planning failures for history rewrite ([`super::HistoryError::Planning`](crate::history::HistoryError::Planning)).

use crate::objects::ObjectId;

/// Failure modes for invalid planning inputs, as opposed to repository read failures
/// (which surface as their own [`Error`](enum@crate::error::Error) area).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum PlanningError {
    /// Replay order construction produced an empty list when a tip was required.
    #[error("empty replay order")]
    EmptyReplayOrder,
    /// The replay tip commit is missing from the oid mapping (internal invariant).
    #[error("replay tip missing from mapping")]
    ReplayTipMissingFromMapping,
    /// Replay order list was empty when a non-empty segment was required.
    #[error("replay order must not be empty")]
    ReplayOrderEmpty,
    /// The same commit oid appeared twice in the replay order.
    #[error("replay order must not list the same commit twice")]
    ReplayOrderDuplicateCommit,
    /// Replay order is not a contiguous first-parent segment from the oldest commit.
    #[error("replay order is not a first-parent segment from oldest")]
    ReplayOrderNotFirstParentSegment,
    /// The oldest commit does not start the replay segment.
    #[error("oldest does not start the replay segment")]
    OldestDoesNotStartReplaySegment,
    /// Replay order covers more or fewer commits than the single first-parent chain.
    #[error("replay order must cover exactly one first-parent segment")]
    ReplayOrderWrongSegmentCoverage,
    /// More than one tip was found in the replay order.
    #[error("replay order must have exactly one segment tip")]
    ReplayOrderMultipleTips,
    /// The computed segment tip is not listed in the replay order.
    #[error("replay segment tip missing from replay order")]
    ReplaySegmentTipMissingFromOrder,
    /// Merge commit replay expected matching parent counts.
    #[error("merge replay parent count mismatch")]
    MergeReplayParentCountMismatch,
    /// Fast-path fold only applies with [`RefScope::Head`](crate::history::RefScope::Head).
    #[error("head-only fold requires RefScope::Head")]
    FoldRequiresHeadScope,
    /// Fold fast path requires the squashed range tip to be `HEAD`.
    #[error("fold fast path requires the range tip to be HEAD")]
    FoldRequiresTipIsHead,
    /// `git rebase` planning found no merge base between upstream and the branch tip.
    #[error(
        "no merge base between upstream {upstream} and tip {tip}",
        upstream = upstream.to_hex(),
        tip = tip.to_hex()
    )]
    NoMergeBaseBetweenUpstreamAndTip {
        /// Upstream (fork) side used for the merge-base test.
        upstream: ObjectId,
        /// Branch tip being rebased.
        tip: ObjectId,
    },
    /// Rebase onto was called without any tip commits.
    #[error("at least one tip commit is required")]
    RebaseTipsRequired,
    /// The fork commit is not an ancestor of a listed tip.
    #[error(
        "fork {fork} is not an ancestor of tip {tip}",
        fork = fork.to_hex(),
        tip = tip.to_hex()
    )]
    ForkNotAncestorOfTip {
        /// Fork (exclude) boundary.
        fork: ObjectId,
        /// Tip that failed the ancestry check.
        tip: ObjectId,
    },
    /// Topological replay order has a cycle (internal invariant).
    #[error("rebase replay order has a cycle among in-range parents")]
    RebaseReplayOrderCycle,
    /// Implicit-upstream rebase requires `HEAD` on a branch.
    #[error("cannot rebase from a non-branch HEAD")]
    RebaseUpstreamRequiresBranchHead,
    /// Implicit-upstream rebase needs an upstream configuration while `HEAD` is detached.
    #[error("cannot rebase detached HEAD without an upstream argument")]
    RebaseDetachedHeadNeedsUpstream,
    /// The current branch has no `@{upstream}` to rebase onto.
    #[error("no upstream configured for branch '{branch}'")]
    NoUpstreamForBranch {
        /// Short branch name (under `refs/heads/`).
        branch: String,
    },
    /// [`super::squash`](crate::history::squash) was called without revision tokens.
    #[error("revision range is required")]
    SquashRevisionRangeRequired,
    /// Squash range parsing requires a `<base>..<tip>`-style walk.
    #[error("not a '<base>..<tip>' revision range")]
    SquashRangeNotDotDot,
    /// Squash range selected no commits.
    #[error("the revision range is empty")]
    SquashRevisionRangeEmpty,
    /// Squash needs at least two commits in the range.
    #[error("the revision range holds a single commit; nothing to squash")]
    SquashSingleCommitInRange,
    /// Unexpected plan shape from the resumable rebase planner.
    #[error("unexpected non-resumable rebase plan")]
    UnexpectedNonResumableRebasePlan,
    /// A rewritten row is missing its replacement oid (internal invariant).
    #[error("rewrite row missing replacement oid")]
    RewriteRowMissingReplacementOid,
    /// Absorb could not find a path in the tree being edited.
    #[error("path '{path}' not found in tree")]
    AbsorbPathNotInTree {
        /// Repository-relative path in storage form.
        path: String,
    },
    /// Fixup selection is missing a path where one is required.
    #[error("fixup selection patch missing path")]
    FixupSelectionPatchMissingPath,
    /// A commit slated for move is absent from the reordered chain.
    #[error("commit missing from reordered chain")]
    MoveCommitMissingFromChain,
    /// Split planning hit an inconsistent diff entry (internal invariant).
    #[error("split planning: added diff entry missing new path")]
    SplitAddedDiffMissingNewPath,
    /// Split planning hit an inconsistent diff entry (internal invariant).
    #[error("split planning: deleted diff entry missing old path")]
    SplitDeletedDiffMissingOldPath,
    /// Split planning hit an inconsistent diff entry (internal invariant).
    #[error("split planning: modified diff entry missing path")]
    SplitModifiedDiffMissingPath,
    /// Split planning hit an inconsistent diff entry (internal invariant).
    #[error("split planning: rename diff entry missing new path")]
    SplitRenameMissingNewPath,
    /// Split planning hit an inconsistent diff entry (internal invariant).
    #[error("split planning: rename diff entry missing old path")]
    SplitRenameMissingOldPath,
    /// Split refuses to apply while an unmerged diff entry is present.
    #[error("unexpected unmerged diff entry in split")]
    SplitUnexpectedUnmergedDiff,
}
