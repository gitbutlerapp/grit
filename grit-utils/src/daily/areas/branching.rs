//! Branching headlines: branch list, merge, pick.

use super::super::helpers::{
    branch_list_bench, ctx, headline_spec, merge_bench, pick_bench, setup_hot_path_pick_merge,
    setup_many_refs_worktree,
};
use super::super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use crate::scenarios::PrepareKind;

fn setup_many_refs(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_many_refs_worktree(setup)?;
    Ok(ctx(repo, setup))
}

fn setup_pick_merge(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    setup_hot_path_pick_merge(setup)
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("branch/list@many-refs"),
            pending: false,
            setup: setup_many_refs,
            prepare: DailyPrepare::None,
            build_git: |c| branch_list_bench(c).1,
            build_grit: |c| branch_list_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: headline_spec("merge/clean@synth-10k"),
            pending: false,
            setup: setup_pick_merge,
            prepare: DailyPrepare::Hook(PrepareKind::MergeReset),
            build_git: |c| merge_bench(c).1,
            build_grit: |c| merge_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: headline_spec("pick/one@synth-10k"),
            pending: false,
            setup: setup_pick_merge,
            prepare: DailyPrepare::Hook(PrepareKind::PickReset),
            build_git: |c| pick_bench(c).1,
            build_grit: |c| pick_bench(c).0,
        },
    ]
}
