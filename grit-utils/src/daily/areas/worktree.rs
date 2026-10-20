//! Worktree headline scenarios on synth-10k (and hot-path for switch/stash).

use super::super::helpers::{
    add_modified_bench, commit_small_bench, ctx, headline_spec, setup_hot_path_switch,
    setup_registry_worktree, setup_stash_repo, stash_bench, status_clean_bench, status_dirty_bench,
    switch_bench, synth10k_kind,
};
use super::super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use crate::scenarios::PrepareKind;

fn setup_synth10k(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_registry_worktree(setup, synth10k_kind(setup.smoke))?;
    Ok(ctx(repo, setup))
}

fn setup_switch(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    setup_hot_path_switch(setup)
}

fn setup_stash(setup: &DailySetup<'_>) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_stash_repo(setup)?;
    Ok(ctx(repo, setup))
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("status/clean@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeStatusClean),
            build_git: |c| status_clean_bench(c).1,
            build_grit: |c| status_clean_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("status/dirty@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeStatusDirty),
            build_git: |c| status_dirty_bench(c).1,
            build_grit: |c| status_dirty_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("add/modified@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeAdd),
            build_git: |c| add_modified_bench(c).1,
            build_grit: |c| add_modified_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("commit/small@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: DailyPrepare::Hook(PrepareKind::WorktreeCommit),
            build_git: |c| commit_small_bench(c).1,
            build_grit: |c| commit_small_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("switch/branch@synth-10k"),
            pending: false,
            setup: setup_switch,
            prepare: DailyPrepare::Hook(PrepareKind::SwitchReset),
            build_git: |c| switch_bench(c).1,
            build_grit: |c| switch_bench(c).0,
            verify: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("stash/push-pop@synth-10k"),
            pending: false,
            setup: setup_stash,
            prepare: DailyPrepare::Hook(PrepareKind::StashReset),
            build_git: |c| stash_bench(c).1,
            build_grit: |c| stash_bench(c).0,
            verify: None,
        },
    ]
}
