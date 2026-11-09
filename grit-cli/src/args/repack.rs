//! Clap arguments for `grit repack`.

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

/// Pack reachable objects and remove redundant packs.
#[derive(Debug, Args)]
pub(crate) struct RepackArgs {
    /// Recompute every delta instead of reusing the ones already in packs.
    #[arg(short, long)]
    pub(crate) force: bool,
    /// Show what would be packed without writing.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Suppress progress summary on stderr.
    #[arg(short, long)]
    pub(crate) quiet: bool,
}
