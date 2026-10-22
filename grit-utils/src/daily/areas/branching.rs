//! Branching matrix scenarios and headline rows.

use super::super::helpers::{
    branch_list_bench, headline_spec, matrix_spec, merge_bench, pick_bench,
    prepare_history_rewrite_reset, prepare_merge_reset, prepare_pick_reset, prepare_revert_reset,
    prepare_switch_reset, setup_git_git_history, setup_hot_path_pick_merge,
};
use super::super::matrix_helpers::{
    branch_create_bench, branch_delete_bench, history_reword_bench,
    merge_100k_bench, pick_100k_bench, prepare_branch_create, prepare_branch_delete,
    prepare_stash_list, prepare_tag_create, revert_bench, setup_branch_create,
    setup_branch_delete, setup_history_reword, setup_hot_path_pick_merge_100k,
    setup_hot_path_revert, setup_hot_path_switch_100k, setup_many_refs, setup_many_refs_loose,
    setup_stash_with_entry, setup_tag_create, stash_list_bench, switch_100k_bench,
    tag_contains_git_git_bench, tag_create_bench, tag_list_bench,
    tag_list_sort_version_git_git_bench, write_git_commit_graph_reachable,
};
use super::super::plan::DailyHeadlinePlan;

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("branch/list@many-refs"),
            pending: false,
            setup: setup_many_refs,
            prepare: super::super::helpers::prepare_none,
            build_git: |c| branch_list_bench(c).1,
            build_grit: |c| branch_list_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("merge/clean@synth-10k"),
            pending: false,
            setup: setup_hot_path_pick_merge,
            prepare: prepare_merge_reset,
            build_git: |c| merge_bench(c).1,
            build_grit: |c| merge_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("pick/one@synth-10k"),
            pending: false,
            setup: setup_hot_path_pick_merge,
            prepare: prepare_pick_reset,
            build_git: |c| pick_bench(c).1,
            build_grit: |c| pick_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("switch/branch@synth-100k"),
            pending: false,
            setup: setup_hot_path_switch_100k,
            prepare: prepare_switch_reset,
            build_git: |c| switch_100k_bench(c).1,
            build_grit: |c| switch_100k_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("branch/create@many-refs"),
            pending: false,
            setup: setup_branch_create,
            prepare: prepare_branch_create,
            build_git: |c| branch_create_bench(c).1,
            build_grit: |c| branch_create_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("branch/delete@many-refs"),
            pending: false,
            setup: setup_branch_delete,
            prepare: prepare_branch_delete,
            build_git: |c| branch_delete_bench(c).1,
            build_grit: |c| branch_delete_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("branch/list@many-refs-loose"),
            pending: false,
            setup: setup_many_refs_loose,
            prepare: super::super::helpers::prepare_none,
            build_git: |c| branch_list_bench(c).1,
            build_grit: |c| branch_list_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("tag/list@many-refs"),
            pending: false,
            setup: setup_many_refs,
            prepare: super::super::helpers::prepare_none,
            build_git: |c| tag_list_bench(c).1,
            build_grit: |c| tag_list_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("tag/create@many-refs"),
            pending: false,
            setup: setup_tag_create,
            prepare: prepare_tag_create,
            build_git: |c| tag_create_bench(c).1,
            build_grit: |c| tag_create_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("merge/clean@synth-100k"),
            pending: false,
            setup: setup_hot_path_pick_merge_100k,
            prepare: prepare_merge_reset,
            build_git: |c| merge_100k_bench(c).1,
            build_grit: |c| merge_100k_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("revert/one@synth-10k"),
            pending: false,
            setup: setup_hot_path_revert,
            prepare: prepare_revert_reset,
            build_git: |c| revert_bench(c).1,
            build_grit: |c| revert_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("pick/one@synth-100k"),
            pending: false,
            setup: setup_hot_path_pick_merge_100k,
            prepare: prepare_pick_reset,
            build_git: |c| pick_100k_bench(c).1,
            build_grit: |c| pick_100k_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("history/reword@synth-10k"),
            pending: false,
            setup: setup_history_reword,
            prepare: prepare_history_rewrite_reset,
            build_git: |c| history_reword_bench(c).1,
            build_grit: |c| history_reword_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("stash/list@synth-10k"),
            pending: false,
            setup: setup_stash_with_entry,
            prepare: prepare_stash_list,
            build_git: |c| stash_list_bench(c).1,
            build_grit: |c| stash_list_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: matrix_spec("tag/list-sort-version@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: super::super::helpers::prepare_none,
            build_git: |c| tag_list_sort_version_git_git_bench(c).1,
            build_grit: |c| tag_list_sort_version_git_git_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: matrix_spec("tag/contains@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_tag_contains_git_git,
            build_git: |c| tag_contains_git_git_bench(c).1,
            build_grit: |c| tag_contains_git_git_bench(c).0,
            correctness: None,
        },
    ]
}

fn prepare_tag_contains_git_git(
    ctx: &super::super::plan::DailyHeadlineContext,
) -> super::super::plan::DailyPrepare {
    let _ = write_git_commit_graph_reachable(&ctx.git, &ctx.repo);
    super::super::helpers::prepare_none(ctx)
}
