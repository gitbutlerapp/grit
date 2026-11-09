//! Clap arguments for `grit pick`.

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

/// Cherry-pick commits onto the current branch.
#[derive(Debug, Args)]
pub(crate) struct PickArgs {
    /// Commits or ranges to pick (`A..B`, branch tips, oids). Omit with `--continue`, `--abort`, or `--skip`.
    #[arg(
            required_unless_present_any = ["continue_pick", "abort", "skip"],
            value_name = "COMMIT",
            conflicts_with_all = ["continue_pick", "abort", "skip"]
        )]
    pub(crate) commits: Vec<String>,
    /// Parent number when picking a merge commit (1-based, long only).
    #[arg(long = "mainline", value_name = "N")]
    pub(crate) mainline: Option<u32>,
    /// Finish a pick stopped on conflicts or an empty commit.
    #[arg(long = "continue", conflicts_with_all = ["abort", "skip"])]
    pub(crate) continue_pick: bool,
    /// Cancel an in-progress pick and restore the pre-pick state.
    #[arg(long = "abort", conflicts_with_all = ["continue_pick", "skip"])]
    pub(crate) abort: bool,
    /// Skip the current pick step and continue with the next commit.
    #[arg(long = "skip", conflicts_with_all = ["continue_pick", "abort"])]
    pub(crate) skip: bool,
    /// Append `(cherry picked from commit …)` to each new commit message.
    #[arg(short = 'x', long = "record-origin")]
    pub(crate) record_origin: bool,
    /// Apply the pick to the index only; do not create a commit yet.
    #[arg(long = "no-commit")]
    pub(crate) no_commit: bool,
    /// When a pick step would not change the tree: stop (default), keep an empty commit, or drop the step.
    #[arg(long = "empty", value_enum, default_value_t = crate::commands::replay_merge_args::EmptyStepArg::Stop)]
    pub(crate) empty: crate::commands::replay_merge_args::EmptyStepArg,
    #[command(flatten)]
    pub(crate) message: message_input::MessageInputArgs,
    /// Predict whether the pick would succeed without changing refs, the index, or the worktree.
    #[arg(
            long = "check",
            conflicts_with_all = ["continue_pick", "abort", "skip", "dry_run"]
        )]
    pub(crate) check: bool,
    /// List commits that would be picked without changing refs or the worktree.
    #[arg(long = "dry-run", conflicts_with = "check")]
    pub(crate) dry_run: bool,
    #[command(flatten)]
    pub(crate) replay_merge: crate::commands::replay_merge_args::ReplayMergeArgs,
}
