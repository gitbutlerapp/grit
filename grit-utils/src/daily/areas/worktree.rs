//! Worktree and related matrix scenarios (status, add, commit, restore, reset, rm, mv, clean, init).

use super::super::helpers::{
    add_all_bench, add_dry_run_bench, add_hunks_bench, add_lines_bench, add_modified_bench,
    add_patch_file_bench, add_patch_file_large_bench, add_update_bench, clean_untracked_bench,
    commit_dry_run_bench, commit_paths_bench, commit_small_bench, commit_staged_bench, ctx,
    headline_spec, init_empty_bench, mv_file_bench, prepare_stash_reset, prepare_switch_reset,
    prepare_worktree_add, prepare_worktree_add_hunks, prepare_worktree_add_lines,
    prepare_worktree_add_patch_file, prepare_worktree_clean_untracked, prepare_worktree_commit,
    prepare_worktree_commit_staged, prepare_worktree_init_empty, prepare_worktree_mv_file,
    prepare_worktree_restore_paths, prepare_worktree_rm_dir, prepare_worktree_status_clean,
    prepare_worktree_status_dirty, reset_hard_bench, reset_mixed_bench, restore_paths_bench,
    rm_dir_bench, scenario_spec, setup_big_blobs, setup_deep_tree, setup_hot_path_switch,
    setup_init_empty, setup_shape_worktree, setup_small, setup_stash_repo, setup_synth100k,
    stash_bench, status_clean_bench, status_dirty_bench, status_paths_bench, status_renamed_bench,
    switch_bench,
};
use super::super::plan::{DailyHeadlinePlan, DailySetup};
use super::super::registry::RepoShape;

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
            prepare: prepare_worktree_status_clean,
            build_git: |c| status_clean_bench(c).1,
            build_grit: |c| status_clean_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("status/dirty@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: prepare_worktree_status_dirty,
            build_git: |c| status_dirty_bench(c).1,
            build_grit: |c| status_dirty_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("add/modified@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: prepare_worktree_add,
            build_git: |c| add_modified_bench(c).1,
            build_grit: |c| add_modified_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("commit/small@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: prepare_worktree_commit,
            build_git: |c| commit_small_bench(c).1,
            build_grit: |c| commit_small_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("switch/branch@synth-10k"),
            pending: false,
            setup: setup_switch,
            prepare: prepare_switch_reset,
            build_git: |c| switch_bench(c).1,
            build_grit: |c| switch_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("stash/push-pop@synth-10k"),
            pending: false,
            setup: setup_stash,
            prepare: prepare_stash_reset,
            build_git: |c| stash_bench(c).1,
            build_grit: |c| stash_bench(c).0,
            correctness: None,
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
        spec: scenario_spec("status/paths@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: prepare_worktree_status_dirty,
        build_git: |c| status_paths_bench(c).1,
        build_grit: |c| status_paths_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("status/renamed@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_mv_file,
        build_git: |c| status_renamed_bench(c).1,
        build_grit: |c| status_renamed_bench(c).0,
        correctness: None,
    });

    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/all@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: prepare_worktree_status_dirty,
        build_git: |c| add_all_bench(c).1,
        build_grit: |c| add_all_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/update@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: prepare_worktree_status_dirty,
        build_git: |c| add_update_bench(c).1,
        build_grit: |c| add_update_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/dry-run@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_status_dirty,
        build_git: |c| add_dry_run_bench(c).1,
        build_grit: |c| add_dry_run_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/patch-file@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_add_patch_file,
        build_git: |c| add_patch_file_bench(c).1,
        build_grit: |c| add_patch_file_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/hunks@large-file"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_add_hunks,
        build_git: |c| add_hunks_bench(c).1,
        build_grit: |c| add_hunks_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/lines@large-file"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_add_lines,
        build_git: |c| add_lines_bench(c).1,
        build_grit: |c| add_lines_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/patch-file@large-file"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_add_hunks,
        build_git: |c| add_patch_file_large_bench(c).1,
        build_grit: |c| add_patch_file_large_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("add/modified@big-blobs"),
        pending: false,
        setup: setup_big_blobs,
        prepare: prepare_worktree_add,
        build_git: |c| add_modified_bench(c).1,
        build_grit: |c| add_modified_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/small@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: prepare_worktree_commit,
        build_git: |c| commit_small_bench(c).1,
        build_grit: |c| commit_small_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/small@small"),
        pending: false,
        setup: setup_small,
        prepare: prepare_worktree_commit,
        build_git: |c| commit_small_bench(c).1,
        build_grit: |c| commit_small_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/staged@synth-100k"),
        pending: false,
        setup: setup_synth100k,
        prepare: prepare_worktree_commit_staged,
        build_git: |c| commit_staged_bench(c).1,
        build_grit: |c| commit_staged_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/paths@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_commit,
        build_git: |c| commit_paths_bench(c).1,
        build_grit: |c| commit_paths_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("commit/dry-run@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_commit,
        build_git: |c| commit_dry_run_bench(c).1,
        build_grit: |c| commit_dry_run_bench(c).0,
        correctness: None,
    });

    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("restore/paths@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_restore_paths,
        build_git: |c| restore_paths_bench(c).1,
        build_grit: |c| restore_paths_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("reset/hard@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_status_dirty,
        build_git: |c| reset_hard_bench(c).1,
        build_grit: |c| reset_hard_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("reset/hard-delete-heavy@wide-48k"),
        pending: false,
        setup: super::super::matrix_helpers::setup_delete_heavy_wide,
        prepare: prepare_switch_reset,
        build_git: |c| super::super::matrix_helpers::reset_hard_delete_heavy_bench(c).1,
        build_grit: |c| super::super::matrix_helpers::reset_hard_delete_heavy_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("reset/mixed@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_status_dirty,
        build_git: |c| reset_mixed_bench(c).1,
        build_grit: |c| reset_mixed_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("reset/undo-commit@git-git"),
        pending: false,
        setup: super::super::helpers::setup_git_git_history,
        prepare: super::super::matrix_helpers::prepare_reset_undo_commit,
        build_git: |c| super::super::helpers::reset_undo_commit_bench(c).1,
        build_grit: |c| super::super::helpers::reset_undo_commit_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("rm/dir@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_rm_dir,
        build_git: |c| rm_dir_bench(c).1,
        build_grit: |c| rm_dir_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("mv/file@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_mv_file,
        build_git: |c| mv_file_bench(c).1,
        build_grit: |c| mv_file_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("clean/untracked@synth-10k"),
        pending: false,
        setup: setup_synth10k,
        prepare: prepare_worktree_clean_untracked,
        build_git: |c| clean_untracked_bench(c).1,
        build_grit: |c| clean_untracked_bench(c).0,
        correctness: None,
    });
    plans.push(DailyHeadlinePlan {
        spec: scenario_spec("init/empty@small"),
        pending: false,
        setup: setup_init_empty,
        prepare: prepare_worktree_init_empty,
        build_git: |c| init_empty_bench(c).1,
        build_grit: |c| init_empty_bench(c).0,
        correctness: None,
    });

    plans
}

fn matrix_status(
    id: &'static str,
    setup: fn(&DailySetup<'_>) -> anyhow::Result<super::super::plan::DailyHeadlineContext>,
    clean: bool,
) -> DailyHeadlinePlan {
    let prepare = if clean {
        prepare_worktree_status_clean
    } else {
        prepare_worktree_status_dirty
    };
    if clean {
        DailyHeadlinePlan {
            spec: scenario_spec(id),
            pending: false,
            setup,
            prepare,
            build_git: |c| status_clean_bench(c).1,
            build_grit: |c| status_clean_bench(c).0,
            correctness: None,
        }
    } else {
        DailyHeadlinePlan {
            spec: scenario_spec(id),
            pending: false,
            setup,
            prepare,
            build_git: |c| status_dirty_bench(c).1,
            build_grit: |c| status_dirty_bench(c).0,
            correctness: None,
        }
    }
}
