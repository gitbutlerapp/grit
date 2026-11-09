//! Clap arguments for `grit rm`.

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

/// Remove tracked paths from the working tree and index.
#[derive(Debug, Args)]
pub(crate) struct RmArgs {
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::PathspecArgs,
    /// Remove from the index only; keep files in the working tree.
    #[arg(long)]
    pub(crate) cached: bool,
    /// Remove even when paths have local modifications.
    #[arg(short = 'f', long = "force")]
    pub(crate) force: bool,
    /// List paths that would be removed without changing the index or work tree.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Treat non-matching pathspecs as success with an empty result.
    #[arg(long = "ignore-unmatch")]
    pub(crate) ignore_unmatch: bool,
}
