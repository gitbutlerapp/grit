//! Clap arguments for `grit hooks`.

use super::HooksAction;
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

/// List hook scripts or run one by name.
#[derive(Debug, Args)]
pub(crate) struct HooksArgs {
    #[command(flatten)]
    pub(crate) limit: limit_args::LimitArgs,
    #[command(subcommand)]
    pub(crate) action: Option<HooksAction>,
}
