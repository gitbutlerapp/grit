//! Clap arguments for `grit integrate`.

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

/// Merge a branch into the server's target branch (checking it in first), then refresh this checkout.
#[derive(Debug, Args)]
pub(crate) struct IntegrateArgs {
    /// The branch to merge.
    #[arg(value_name = "BRANCH")]
    pub(crate) branch: String,
    /// For a shared branch: integrate without waiting for this member's
    /// changes, which stay in their copy. Repeatable.
    #[arg(long, value_name = "USER")]
    pub(crate) without: Vec<String>,
}
