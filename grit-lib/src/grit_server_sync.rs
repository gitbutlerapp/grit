//! Outcome classification for syncing a clone with a grit hosting space.
//!
//! These types describe per-branch sync state without CLI or JSON details so
//! callers (including `grit sync`) can derive exit-related flags in one place.

/// What happened to the target branch during a sync.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTargetAction {
    /// Already matched the upstream project.
    UpToDate,
    /// Moved forward to the upstream tip.
    FastForwarded,
    /// Created locally from the upstream tip.
    Created,
    /// Local commits are not on the upstream branch.
    Ahead,
    /// Local and upstream have diverged.
    Diverged,
}

/// What happened to one branch during a sync.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncBranchAction {
    /// Pushed to the member's space.
    Pushed,
    /// Fast-forwarded to the space's tip.
    Pulled,
    /// Created locally from the space's tip.
    Created,
    /// Already matched the space.
    UpToDate,
    /// Local and space tips diverged.
    Diverged,
    /// The server refused the push.
    Refused,
}

/// Per-branch sync state used to classify the overall report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncBranchState {
    /// What happened to this branch.
    pub action: SyncBranchAction,
    /// A shared branch pushed again after integration (`pushed_to_integrated`).
    pub integrated: bool,
    /// Any shared head whose divergence needs a human decision.
    pub shared_needs_decision: bool,
}

/// Inputs for [`sync_needs_attention`] and [`sync_needs_decision`].
#[non_exhaustive]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncReportSummary {
    /// Target branch action, if a target was synced.
    pub target: Option<SyncTargetAction>,
    /// One entry per active branch considered during sync.
    pub branches: Vec<SyncBranchState>,
}

impl SyncReportSummary {
    /// Build a summary from sync target and per-branch state.
    #[must_use]
    pub fn new(target: Option<SyncTargetAction>, branches: Vec<SyncBranchState>) -> Self {
        Self { target, branches }
    }
}

/// Whether any branch could not be synced (diverged or refused), or the target diverged.
#[must_use]
pub fn sync_needs_attention(summary: &SyncReportSummary) -> bool {
    summary
        .target
        .is_some_and(|t| t == SyncTargetAction::Diverged)
        || summary.branches.iter().any(|b| {
            matches!(
                b.action,
                SyncBranchAction::Diverged | SyncBranchAction::Refused
            )
        })
}

/// Whether a shared head needs a decision or a branch was pushed after integration.
#[must_use]
pub fn sync_needs_decision(summary: &SyncReportSummary) -> bool {
    summary
        .branches
        .iter()
        .any(|b| b.integrated || b.shared_needs_decision)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attention_when_target_or_branch_diverged_or_refused() {
        assert!(!sync_needs_attention(&SyncReportSummary::new(
            Some(SyncTargetAction::FastForwarded),
            vec![],
        )));
        assert!(sync_needs_attention(&SyncReportSummary::new(
            Some(SyncTargetAction::Diverged),
            vec![],
        )));
        assert!(sync_needs_attention(&SyncReportSummary::new(
            None,
            vec![SyncBranchState {
                action: SyncBranchAction::Refused,
                integrated: false,
                shared_needs_decision: false,
            }],
        )));
    }

    #[test]
    fn decision_when_integrated_or_shared_head() {
        assert!(!sync_needs_decision(&SyncReportSummary::new(
            None,
            vec![SyncBranchState {
                action: SyncBranchAction::Pushed,
                integrated: false,
                shared_needs_decision: false,
            }],
        )));
        assert!(sync_needs_decision(&SyncReportSummary::new(
            None,
            vec![SyncBranchState {
                action: SyncBranchAction::Pulled,
                integrated: true,
                shared_needs_decision: false,
            }],
        )));
        assert!(sync_needs_decision(&SyncReportSummary::new(
            None,
            vec![SyncBranchState {
                action: SyncBranchAction::UpToDate,
                integrated: false,
                shared_needs_decision: true,
            }],
        )));
    }
}
