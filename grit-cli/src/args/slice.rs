//! Clap arguments for `grit slice`.

use super::SliceAction;
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

/// Named sets of paths to check out together, kept on the server's target branch.
#[derive(Debug, Args)]
pub(crate) struct SliceArgs {
    #[command(subcommand)]
    pub(crate) action: SliceAction,
    /// The repository whose slices to use (default: the checkout you are in).
    #[arg(long, global = true, value_name = "URL")]
    pub(crate) url: Option<String>,
}
