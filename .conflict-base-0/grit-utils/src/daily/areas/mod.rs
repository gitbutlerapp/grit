//! Headline scenario registry grouped by functional area (matrix modules).

pub mod branching;
pub mod history;
pub mod maintenance;
pub mod misc;
pub mod refs;
pub mod remotes;
pub mod revision_selection;
pub mod worktree;

use super::plan::DailyHeadlinePlan;

/// All executable daily plans (headlines + matrix) in stable id order.
pub fn all_daily_plans() -> Vec<DailyHeadlinePlan> {
    let mut plans = Vec::new();
    plans.extend(worktree::plans());
    plans.extend(misc::plans());
    plans.extend(history::plans());
    plans.extend(revision_selection::plans());
    plans.extend(branching::plans());
    plans.extend(remotes::plans());
    plans.extend(refs::plans());
    plans.extend(maintenance::plans());
    plans
}

/// Headline-only subset (17 cooking-page rows).
pub fn all_headline_plans() -> Vec<DailyHeadlinePlan> {
    all_daily_plans()
        .into_iter()
        .filter(|p| p.spec.headline)
        .collect()
}
