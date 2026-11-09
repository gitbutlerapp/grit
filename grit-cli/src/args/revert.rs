//! Clap arguments for `grit revert`.

use crate::branch_list_args;
use crate::clap_value_args;
use crate::diff_args;
use crate::diff_format;
use crate::global_args;
use crate::limit_args;
use crate::merge_strategy_args;
use crate::message_input;
use crate::pathspec_args;
use crate::quiet_args;
use crate::rev_select;
use clap::Args;

/// Undo commits with new inverse commits.
#[derive(Debug, Args)]
pub(crate) struct RevertArgs {
    /// Commits or revision ranges to revert (newest first within each range).
    #[arg(
            required_unless_present_any = ["continue_revert", "abort", "skip"],
            value_name = "COMMIT"
        )]
    pub(crate) commits: Vec<String>,
    /// Parent number when reverting a merge commit (1-based, long only).
    #[arg(long = "mainline", value_name = "N")]
    pub(crate) mainline: Option<u32>,
    /// Finish a revert stopped on conflicts or an empty commit.
    #[arg(long = "continue", conflicts_with_all = ["abort", "skip"])]
    pub(crate) continue_revert: bool,
    /// Cancel an in-progress revert and restore the pre-revert state.
    #[arg(long = "abort", conflicts_with_all = ["continue_revert", "skip"])]
    pub(crate) abort: bool,
    /// Skip the current revert step and continue with the next commit.
    #[arg(long = "skip", conflicts_with_all = ["continue_revert", "abort"])]
    pub(crate) skip: bool,
    /// Apply the revert to the index only; do not create a commit yet.
    #[arg(long = "no-commit")]
    pub(crate) no_commit: bool,
    /// When a revert step would not change the tree: stop (default), keep an empty commit, or drop the step.
    #[arg(long = "empty", value_enum, default_value_t = crate::commands::replay_merge_args::EmptyStepArg::Stop)]
    pub(crate) empty: crate::commands::replay_merge_args::EmptyStepArg,
    #[command(flatten)]
    pub(crate) message: message_input::MessageInputArgs,
    /// Predict whether the revert would succeed without changing refs, the index, or the worktree.
    #[arg(
            long = "check",
            conflicts_with_all = ["continue_revert", "abort", "skip", "dry_run"]
        )]
    pub(crate) check: bool,
    /// List commits that would be reverted without changing refs or the worktree.
    #[arg(long = "dry-run", conflicts_with = "check")]
    pub(crate) dry_run: bool,
    #[command(flatten)]
    pub(crate) replay_merge: crate::commands::replay_merge_args::ReplayMergeArgs,
}
