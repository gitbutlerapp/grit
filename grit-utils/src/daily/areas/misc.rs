//! Diff matrix scenarios on registry fixtures.

use super::super::helpers::{
    ctx, diff_staged_bench, diff_worktree_bench, headline_spec, scenario_spec, setup_big_blobs,
    setup_deep_tree, setup_registry_worktree, setup_synth100k, synth10k_kind,
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
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("diff/worktree@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeDiff),
            build_git: |c| diff_worktree_bench(c).1,
            build_grit: |c| diff_worktree_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("diff/worktree@synth-100k"),
            pending: false,
            setup: setup_synth100k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeDiff),
            build_git: |c| diff_worktree_bench(c).1,
            build_grit: |c| diff_worktree_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("diff/worktree@deep-tree"),
            pending: false,
            setup: setup_deep_tree,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeDiff),
            build_git: |c| diff_worktree_bench(c).1,
            build_grit: |c| diff_worktree_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("diff/worktree@big-blobs"),
            pending: false,
            setup: setup_big_blobs,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeDiff),
            build_git: |c| diff_worktree_bench(c).1,
            build_grit: |c| diff_worktree_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("diff/staged@synth-100k"),
            pending: false,
            setup: setup_synth100k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeStagedDiff),
            build_git: |c| diff_staged_bench(c).1,
            build_grit: |c| diff_staged_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("diff/staged@deep-tree"),
            pending: false,
            setup: setup_deep_tree,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeStagedDiff),
            build_git: |c| diff_staged_bench(c).1,
            build_grit: |c| diff_staged_bench(c).0,
            verify: None,
        },
    ]
}
