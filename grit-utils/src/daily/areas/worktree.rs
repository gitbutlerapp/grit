//! Worktree and related matrix scenarios (status, add, commit, restore, reset, rm, mv, clean, init).

use super::super::helpers::{
    add_all_bench, add_modified_bench, clean_untracked_bench, commit_small_bench, ctx,
    headline_spec, init_empty_bench, mv_file_bench, reset_hard_bench, reset_mixed_bench,
    restore_paths_bench, rm_dir_bench, scenario_spec, setup_big_blobs, setup_deep_tree,
    setup_hot_path_switch, setup_init_empty, setup_shape_worktree, setup_small, setup_stash_repo,
    setup_synth100k, stash_bench, status_clean_bench, status_dirty_bench, switch_bench,
};
use super::super::plan::{DailyHeadlinePlan, DailyPrepare, DailySetup};
use super::super::registry::RepoShape;
use crate::scenarios::PrepareKind;

fn setup_synth10k(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    setup_shape_worktree(setup, RepoShape::Synth10k)
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
    let mut plans = vec![
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
    ];

    plans.extend([
        matrix_status("status/clean@synth-100k", setup_synth100k, true),
        matrix_status("status/clean@deep-tree", setup_deep_tree, true),
        matrix_status("status/clean@small", setup_small, true),
        matrix_status("status/dirty@synth-100k", setup_synth100k, false),
        matrix_status("status/dirty@deep-tree", setup_deep_tree, false),
        matrix_status("status/dirty@small", setup_small, false),
    ]);

    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/all@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeStatusDirty),
        build_git: |c| add_all_bench(c).1,
        build_grit: |c| add_all_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/modified@big-blobs"),
        pending: false,
        setup: setup_big_blobs,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeAdd),
        build_git: |c| add_modified_bench(c).1,
        build_grit: |c| add_modified_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/small@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeCommit),
        build_git: |c| commit_small_bench(c).1,
        build_grit: |c| commit_small_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/small@small"),
        pending: false,
        setup: setup_small,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeCommit),
        build_git: |c| commit_small_bench(c).1,
        build_grit: |c| commit_small_bench(c).0,
    });

    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("restore/paths@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeRestorePaths),
        build_git: |c| restore_paths_bench(c).1,
        build_grit: |c| restore_paths_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("reset/hard@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeStatusDirty),
        build_git: |c| reset_hard_bench(c).1,
        build_grit: |c| reset_hard_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("reset/mixed@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeStatusDirty),
        build_git: |c| reset_mixed_bench(c).1,
        build_grit: |c| reset_mixed_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("rm/dir@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeRmDir),
        build_git: |c| rm_dir_bench(c).1,
        build_grit: |c| rm_dir_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("mv/file@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeMvFile),
        build_git: |c| mv_file_bench(c).1,
        build_grit: |c| mv_file_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("clean/untracked@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: DailyPrepare::Hook(PrepareKind::WorktreeCleanUntracked),
        build_git: |c| clean_untracked_bench(c).1,
        build_grit: |c| clean_untracked_bench(c).0,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("init/empty@small"),
        pending: false,
        setup: setup_init_empty,
        prepare: DailyPrepare::None,
        build_git: |c| init_empty_bench(c).1,
        build_grit: |c| init_empty_bench(c).0,
    });

    plans
}

fn matrix_status(
    id: &'static str,
    setup: fn(&DailySetup<'_>) -> anyhow::Result<super::super::plan::DailyHeadlineContext>,
    clean: bool,
) -> DailyHeadlinePlan {
    let prepare = if clean {
        DailyPrepare::Hook(PrepareKind::WorktreeStatusClean)
    } else {
        DailyPrepare::Hook(PrepareKind::WorktreeStatusDirty)
    };
    if clean {
        DailyHeadlinePlan {
            spec: scenario_spec(id),
            pending: false,
            setup,
            prepare,
            build_git: |c| status_clean_bench(c).1,
            build_grit: |c| status_clean_bench(c).0,
        }
    } else {
        DailyHeadlinePlan {
            spec: scenario_spec(id),
            pending: false,
            setup,
            prepare,
            build_git: |c| status_dirty_bench(c).1,
            build_grit: |c| status_dirty_bench(c).0,
        }
    }
}
