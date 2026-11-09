//! Clap arguments for `grit gc`.

use super::GcAction;
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

/// Pack refs, expire reflogs, repack, prune, and refresh commit-graph.
#[derive(Debug, Args)]
pub(crate) struct GcArgs {
    /// Show what would run without writing.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Run only when `gc.auto` or `gc.autoPackLimit` is exceeded (what commands run on their own).
    #[arg(long = "auto")]
    pub(crate) auto: bool,
    /// Suppress progress summary on stderr.
    #[arg(short, long)]
    pub(crate) quiet: bool,
    #[command(subcommand)]
    pub(crate) action: Option<GcAction>,
}
