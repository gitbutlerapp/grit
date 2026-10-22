//! Shared integration-test helpers (refs backend harness, etc.).

mod attributes_harness;
mod config_oracle;
mod diff_harness;
mod history_harness;
mod index_ext_harness;
mod index_harness;
mod merge_harness;
mod refs_harness;
mod refstore_conformance;
mod revparse_oracle;
mod revparse_seed;
mod revwalk_fixtures;

pub use attributes_harness::*;
pub use config_oracle::*;
pub use diff_harness::*;
pub use history_harness::{
    assert_fixup_tip_matches_git_oracle, assert_fixup_tip_structure_matches,
    assert_grit_commits_match_git, assert_rev_list_commits_match_git, build_commit_dag,
    commit_file, commit_labeled_with_dates, disable_grit_change_ids_for_git_oracle,
    expand_revision_tokens, expand_token_for_rev_list, git, git_commit_graph_write,
    git_history_command_available, git_history_fixup_legacy_oracle, git_history_fixup_or_legacy,
    git_history_stderr_indicates_unavailable, git_history_subcommand_available, git_ls_files_stage,
    git_ls_files_stage0_map, git_merge_base, git_merge_base_is_ancestor, git_merge_expect_conflict,
    git_rev_list, git_rev_list_lines, git_rev_list_object_set, git_rev_list_with_boundary,
    git_rev_parse, git_rev_parse_verify, git_version, git_version_at_least, grit_rev_list_commits,
    grit_rev_list_commits_in_repo, grit_rev_list_object_set, harness_committer_line,
    open_grit_repo, ordering_from_git_args, reset_harness_commit_clock, rev_parse_many,
    skip_unless_git_at_least, HistoryRepo,
};
pub use index_ext_harness::*;
pub use index_harness::{
    assert_git_fsck_strict, assert_grit_index_matches_git_ls_files,
    assert_porcelain_v2_matches_git, assert_skip_worktree_matches_git,
    assert_skip_worktree_matches_tag_oracle, clear_all_skip_worktree, git_ls_files_tag_map,
    git_ls_files_verbose_map, grit_skip_worktree_map, seed_multidir_repo,
};
pub use merge_harness::{
    assert_merge_commits_match_git, assert_merge_commits_match_git_with_favor,
    assert_merge_output_matches_oracle, commit_tree, empty_tree, git_commit_tree, git_hash_object,
    git_merge_tree_oracle, git_merge_tree_oracle_with_options, git_write_blob,
    grit_merge_result_tree, skip_merge_tree_git, CommitMergeLabels, GitMergeTreeOracle,
    GitUnmergedRow,
};
pub use refs_harness::*;
pub use refstore_conformance::*;
pub use revparse_oracle::*;
pub use revparse_seed::*;
pub use revwalk_fixtures::{
    setup_dual_merge_back_simplify_oracle, setup_first_parent_boundary_history,
    setup_path_simplify_history, setup_treesame_history,
};
