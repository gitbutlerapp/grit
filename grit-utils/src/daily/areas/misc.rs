//! Non-worktree local scenarios (diff on synth-10k).

use super::super::helpers::{
    ctx, diff_worktree_bench, headline_spec, setup_registry_worktree, synth10k_kind,
};
use super::super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use crate::scenarios::PrepareKind;

fn setup_synth10k(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_registry_worktree(setup, synth10k_kind(setup.smoke))?;
    Ok(ctx(repo, setup))
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![DailyHeadlinePlan {
        spec: headline_spec("diff/worktree@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeDiff),
        build_git: |c| diff_worktree_bench(c).1,
        build_grit: |c| diff_worktree_bench(c).0,
    }]
}
