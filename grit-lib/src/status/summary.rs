//! Dashboard summary types for [`super::Status::assemble_summary`].

use crate::branch_tracking::{
    status_integration_hint, BranchStanding, StandingOptions, StatusHeader, StatusIntegrationHint,
};
use crate::error::Result;
use crate::porcelain::status::{ConflictGroup, StatusGuidance, StatusModel};
use crate::repo::Repository;
use crate::state::InProgressOperation;

/// Options for [`super::Status::assemble_summary`].
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct StatusSummaryOptions {
    /// How far ahead to walk and whether to compute ahead/behind counts.
    pub standing: StandingOptions,
}

impl StatusSummaryOptions {
    /// Set how far ahead to walk and whether to compute ahead/behind counts.
    #[must_use]
    pub fn standing(mut self, standing: StandingOptions) -> Self {
        self.standing = standing;
        self
    }
}

impl Default for StatusSummaryOptions {
    fn default() -> Self {
        Self {
            standing: StandingOptions {
                newest_ahead_limit: 10,
                counts: true,
            },
        }
    }
}

/// Typed dashboard outcome: worktree model plus branch standing and follow-up guidance.
///
/// Built by [`super::Status::assemble_summary`] after [`super::Status::run`].
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct StatusSummary {
    /// Index and worktree scan (staged, unstaged, conflicts, operation state).
    pub model: StatusModel,
    /// Target/upstream comparison for the current branch.
    pub standing: BranchStanding,
    /// Resolved header line (branch standing vs in-progress operation).
    pub header: StatusHeader,
    /// Suggested next steps for the user or agent.
    pub guidance: StatusGuidance,
    /// Unmerged paths grouped by conflict kind.
    pub conflict_groups: Vec<ConflictGroup>,
    /// When the branch should merge or pull to integrate, which ref to use.
    pub integration_hint: Option<StatusIntegrationHint>,
    /// Paused operations detected in the repository (merge, rebase, …).
    pub in_progress: Vec<InProgressOperation>,
}

/// Derive dashboard fields from a [`StatusModel`] without re-scanning the worktree.
pub(crate) fn assemble_status_summary(
    repo: &Repository,
    model: StatusModel,
    options: StatusSummaryOptions,
) -> Result<StatusSummary> {
    let in_progress = repo.operation().in_progress();
    let standing = BranchStanding::compute(repo, &model.head, options.standing)?;
    let header = StatusHeader::resolve(&standing, &model.state, &in_progress);
    let conflict_groups = model.conflict_groups();
    let guidance = model.guidance(&standing);
    let integration_hint = standing
        .branch
        .as_deref()
        .and_then(|branch| status_integration_hint(&standing, Some((repo, branch))));
    Ok(StatusSummary {
        model,
        standing,
        header,
        guidance,
        conflict_groups,
        integration_hint,
        in_progress,
    })
}
