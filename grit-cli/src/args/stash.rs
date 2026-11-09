//! Clap arguments for `grit stash`.

use super::StashAction;
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

/// Save or restore shelved work.
#[derive(Debug, Args)]
pub(crate) struct StashArgs {
    /// Stash message (bare `grit stash` / `grit stash push`).
    #[arg(
        short = 'm',
        long = "message",
        global = true,
        allow_hyphen_values = true
    )]
    pub(crate) message: Option<String>,
    /// Include untracked files (bare `grit stash` / `grit stash push`).
    #[arg(
            short = 'u',
            long = "include-untracked",
            global = true,
            conflicts_with_all = ["hunks", "lines"]
        )]
    pub(crate) include_untracked: bool,
    /// Stash only unstaged hunks from `grit diff --json` (comma-separated, repeatable).
    #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            global = true,
            conflicts_with = "include_untracked"
        )]
    pub(crate) hunks: Vec<String>,
    /// Stash only unstaged line ranges (`path:start-end`, same as `grit add --lines`).
    #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            global = true,
            conflicts_with = "include_untracked"
        )]
    pub(crate) lines: Vec<String>,
    /// Preview stash without updating refs, index, or worktree.
    #[arg(long = "dry-run", global = true)]
    pub(crate) dry_run: bool,
    /// Stash only index changes vs `HEAD` (unstaged edits on other paths remain).
    #[arg(
            short = 'S',
            long = "staged",
            global = true,
            conflicts_with_all = ["keep_index", "hunks", "lines", "include_untracked"]
        )]
    pub(crate) staged: bool,
    /// After stashing, leave the index and worktree matching staged content.
    #[arg(
            short = 'k',
            long = "keep-index",
            global = true,
            conflicts_with_all = ["staged", "hunks", "lines"]
        )]
    pub(crate) keep_index: bool,
    /// Cap path lists in `--dry-run` preview output.
    #[arg(short = 'n', long = "limit", global = true, value_name = "N")]
    pub(crate) stash_limit: Option<usize>,
    #[command(flatten)]
    pub(crate) pathspecs: crate::pathspec_args::TrailingPathspecArgs,
    #[command(subcommand)]
    pub(crate) action: Option<StashAction>,
}
