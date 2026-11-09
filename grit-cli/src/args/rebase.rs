//! Clap arguments for `grit rebase`.

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

/// Replay local commits onto a new base (atomic ref update).
#[derive(Debug, Args)]
pub(crate) struct RebaseArgs {
    /// Commits to replay are those reachable from `HEAD` but not from this revision (defaults to `@{upstream}`).
    #[arg(value_name = "UPSTREAM")]
    pub(crate) upstream: Option<String>,
    /// Replay onto this commit instead of `UPSTREAM`.
    #[arg(long = "onto", value_name = "REV")]
    pub(crate) onto: Option<String>,
    /// Finish a rebase stopped on conflicts after resolving and staging paths.
    #[arg(long = "continue", conflicts_with_all = ["abort", "skip", "upstream", "onto", "check"])]
    pub(crate) continue_rebase: bool,
    /// Cancel an in-progress rebase and restore the branch to its pre-rebase tip.
    #[arg(long = "abort", conflicts_with_all = ["continue_rebase", "skip", "upstream", "onto", "check"])]
    pub(crate) abort: bool,
    /// Skip the current commit and replay the rest.
    #[arg(long = "skip", conflicts_with_all = ["continue_rebase", "abort", "upstream", "onto", "check"])]
    pub(crate) skip: bool,
    #[command(flatten)]
    pub(crate) rebase_flags: crate::commands::history::RebaseRunFlags,
    #[command(flatten)]
    pub(crate) shared: crate::commands::history::HistoryRewriteArgs,
}
