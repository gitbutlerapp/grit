//! Revwalk and blame headlines plus history matrix scenarios.

use super::super::helpers::{
    blame_file_bench, cat_blob_bench, ctx, finish_git_git_matrix_ctx, headline_spec, log_all_bench,
    log_json_bench, log_json_filter_bench, log_json_n10_decorations_bench,
    log_limit_documentation_git_git_bench, log_oneline_bench, log_path_bench, log_stat_n50_bench,
    merge_base_bench, patch_id_head_bench, prepare_history_rewrite_reset, prepare_none,
    reflog_list_bench, resolve_change_id_bench, resolve_head_bench, revs_count_bench,
    revs_count_literal_path_git_git_bench, scenario_spec, setup_big_blobs, setup_git_git_history,
    setup_git_git_loose_tags, setup_git_git_no_commit_graph, setup_git_git_packed_tags,
    setup_git_git_worktree, setup_registry_worktree, setup_shape_worktree, setup_synth100k,
    sh_devnull, shortlog_ahead_bench, shortlog_behind_bench, show_head_bench, synth10k_kind,
};
use super::super::matrix_helpers::{
    prepare_rebase_git_git_reset, rebase_onto_bench, rebase_onto_git_git_bench,
    setup_history_reword, verify_git_commit_graph_in_repo, write_git_commit_graph_reachable,
};
use super::super::plan::{DailyHeadlinePlan, DailySetup};
use super::super::registry::RepoShape;
use crate::campaign_worktree::blame_target_path;

fn setup_git_git(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_git_git_worktree(setup)?;
    let mut headline = ctx(repo, setup);
    headline.blame_path = Some(blame_target_path(setup.git, &headline.repo)?);
    headline.matrix_path = headline.blame_path.clone();
    finish_git_git_matrix_ctx(&mut headline, setup.git)?;
    Ok(headline)
}

fn setup_synth10k(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let repo = setup_registry_worktree(setup, synth10k_kind(setup.smoke))?;
    Ok(ctx(repo, setup))
}

fn setup_git_git_commit_graph(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let headline = setup_git_git_history(setup)?;
    write_git_commit_graph_reachable(setup.git, &headline.repo)?;
    verify_git_commit_graph_in_repo(setup.git, &headline.repo)?;
    Ok(headline)
}

fn setup_deep_tree_blame(
    setup: &DailySetup<'_>,
) -> anyhow::Result<super::super::plan::DailyHeadlineContext> {
    let mut headline = setup_shape_worktree(setup, RepoShape::DeepTree)?;
    headline.blame_path = Some(
        blame_target_path(setup.git, &headline.repo).unwrap_or_else(|_| "deep/file.txt".into()),
    );
    Ok(headline)
}

pub fn plans() -> Vec<DailyHeadlinePlan> {
    vec![
        DailyHeadlinePlan {
            spec: headline_spec("log/oneline-100@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            build_git: |c| log_oneline_bench(c).1,
            build_grit: |c| log_oneline_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("log/json-100@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            build_git: |c| log_json_bench(c).1,
            build_grit: |c| log_json_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("show/head@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            build_git: |c| show_head_bench(c).1,
            build_grit: |c| show_head_bench(c).0,
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: headline_spec("blame/file@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            build_git: |c| {
                let path = c.blame_path.as_deref().unwrap_or("README.md");
                blame_file_bench(c, path).1
            },
            build_grit: |c| {
                let path = c.blame_path.as_deref().unwrap_or("README.md");
                blame_file_bench(c, path).0
            },
            correctness: None,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/oneline-100@synth-100k"),
            pending: false,
            setup: setup_synth100k,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_oneline_bench(c).1,
            build_grit: |c| log_oneline_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/all@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_all_bench(c).1,
            build_grit: |c| log_all_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/path@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_path_bench(c).1,
            build_grit: |c| log_path_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/json-n10-loose-tags@git-git"),
            pending: false,
            setup: setup_git_git_loose_tags,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_json_n10_decorations_bench(c).1,
            build_grit: |c| log_json_n10_decorations_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/json-n10-packed-tags@git-git"),
            pending: false,
            setup: setup_git_git_packed_tags,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_json_n10_decorations_bench(c).1,
            build_grit: |c| log_json_n10_decorations_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/stat-n50@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_stat_n50_bench(c).1,
            build_grit: |c| log_stat_n50_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/json-filter@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_json_filter_bench(c).1,
            build_grit: |c| log_json_filter_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("show/head@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| show_head_bench(c).1,
            build_grit: |c| show_head_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("show/head@big-blobs"),
            pending: false,
            setup: setup_big_blobs,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| show_head_bench(c).1,
            build_grit: |c| show_head_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("blame/file@deep-tree"),
            pending: false,
            setup: setup_deep_tree_blame,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| {
                let path = c.blame_path.as_deref().unwrap_or("README.md");
                blame_file_bench(c, path).1
            },
            build_grit: |c| {
                let path = c.blame_path.as_deref().unwrap_or("README.md");
                blame_file_bench(c, path).0
            },
        },
        DailyHeadlinePlan {
            spec: scenario_spec("revs/count@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| revs_count_bench(c).1,
            build_grit: |c| revs_count_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("revs/count-path@git-git"),
            pending: false,
            setup: setup_git_git_no_commit_graph,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| revs_count_literal_path_git_git_bench(c).1,
            build_grit: |c| revs_count_literal_path_git_git_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("revs/count-range-commit-graph@git-git"),
            pending: false,
            setup: setup_git_git_commit_graph,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| sh_devnull(&c.git, &["rev-list", "--count", "HEAD~20..HEAD"]),
            build_grit: |c| sh_devnull(&c.grit, &["revs", "--count", "HEAD~20..HEAD"]),
        },
        DailyHeadlinePlan {
            spec: scenario_spec("log/limit-path@git-git"),
            pending: false,
            setup: setup_git_git_no_commit_graph,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| log_limit_documentation_git_git_bench(c).1,
            build_grit: |c| log_limit_documentation_git_git_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("merge-base/branches@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| merge_base_bench(c).1,
            build_grit: |c| merge_base_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("resolve/head-n@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| resolve_head_bench(c).1,
            build_grit: |c| resolve_head_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("resolve/change-id@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| resolve_change_id_bench(c).1,
            build_grit: |c| resolve_change_id_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("cat/blob@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| cat_blob_bench(c).1,
            build_grit: |c| cat_blob_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("reflog/list@synth-10k"),
            pending: false,
            setup: setup_synth10k,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| reflog_list_bench(c).1,
            build_grit: |c| reflog_list_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("shortlog/ahead@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| shortlog_ahead_bench(c).1,
            build_grit: |c| shortlog_ahead_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("shortlog/behind@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| shortlog_behind_bench(c).1,
            build_grit: |c| shortlog_behind_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("patch-id/commit@git-git"),
            pending: false,
            setup: setup_git_git,
            prepare: prepare_none,
            correctness: None,
            build_git: |c| patch_id_head_bench(c).1,
            build_grit: |c| patch_id_head_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("rebase/onto@synth-10k"),
            pending: false,
            setup: setup_history_reword,
            prepare: prepare_history_rewrite_reset,
            correctness: None,
            build_git: |c| rebase_onto_bench(c).1,
            build_grit: |c| rebase_onto_bench(c).0,
        },
        DailyHeadlinePlan {
            spec: scenario_spec("rebase/onto@git-git"),
            pending: false,
            setup: setup_git_git_history,
            prepare: prepare_rebase_git_git_reset,
            correctness: None,
            build_git: |c| rebase_onto_git_git_bench(c).1,
            build_grit: |c| rebase_onto_git_git_bench(c).0,
        },
    ]
}
