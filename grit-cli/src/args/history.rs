//! Clap arguments for `grit history`.

use super::HistoryAction;
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

/// Rewrite commits without a sequencer (plan, then apply refs atomically).
#[derive(Debug, Args)]
pub(crate) struct HistoryArgs {
    #[command(subcommand)]
    pub(crate) action: HistoryAction,
}
