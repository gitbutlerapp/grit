//! Clap arguments for `grit merge`.

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

/// Merge another branch, tag or commit into the current branch.
#[derive(Debug, Args)]
pub(crate) struct MergeArgs {
    /// Branch, tag or commit to merge (any revision, e.g. `v1.2`, a commit id, `topic~2`).
    #[arg(
            value_name = "REV",
            required_unless_present_any = ["abort", "continue_merge"],
            conflicts_with = "continue_merge"
        )]
    pub(crate) branch: Option<String>,
    /// Cancel an in-progress merge and restore the pre-merge state.
    #[arg(long = "abort", conflicts_with_all = ["continue_merge", "check"])]
    pub(crate) abort: bool,
    /// Finish an in-progress merge after resolving conflicts (or after `--no-commit`).
    #[arg(long = "continue", conflicts_with_all = ["abort", "check"])]
    pub(crate) continue_merge: bool,
    /// Predict whether the merge would succeed without changing refs, index, or worktree.
    #[arg(long = "check", conflicts_with_all = ["abort", "continue_merge", "dry_run"])]
    pub(crate) check: bool,
    /// Report the planned merge outcome without changing refs, index, or worktree.
    #[arg(long = "dry-run", conflicts_with_all = ["abort", "continue_merge", "check"])]
    pub(crate) dry_run: bool,
    /// Refuse when a fast-forward is not possible (exit code 128).
    #[arg(
            long = "ff-only",
            conflicts_with_all = ["no_ff", "continue_merge", "check", "dry_run"]
        )]
    pub(crate) ff_only: bool,
    /// Record a merge commit even when a fast-forward is possible.
    #[arg(
            long = "no-ff",
            conflicts_with_all = ["ff_only", "continue_merge", "check", "dry_run"]
        )]
    pub(crate) no_ff: bool,
    /// Apply the merge to the index and worktree but do not commit yet.
    #[arg(
            long = "no-commit",
            conflicts_with_all = ["squash", "continue_merge", "check", "dry_run"]
        )]
    pub(crate) no_commit: bool,
    /// Squash merged commits into staged changes and write `SQUASH_MSG`.
    #[arg(
            long = "squash",
            conflicts_with_all = ["no_commit", "continue_merge", "check", "dry_run"]
        )]
    pub(crate) squash: bool,
    /// Merge histories that share no commits (uses the empty tree as merge base).
    #[arg(long = "allow-unrelated-histories")]
    pub(crate) allow_unrelated_histories: bool,
    /// Skip the `pre-merge-commit` and `commit-msg` hooks (`prepare-commit-msg` and `post-merge` still run).
    #[arg(
            long = "no-verify",
            conflicts_with_all = ["abort", "continue_merge", "check", "dry_run"]
        )]
    pub(crate) no_verify: bool,
    #[command(flatten)]
    pub(crate) strategy: merge_strategy_args::MergeStrategyArgs,
    #[command(flatten)]
    pub(crate) message: message_input::MessageInputArgs,
}
