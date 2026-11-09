//! Clap arguments for `grit share`.

use super::ShareAction;
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

/// Work on a branch together with other users of the server: each has their own head of it.
#[derive(Debug, Args)]
pub(crate) struct ShareArgs {
    #[command(subcommand)]
    pub(crate) action: ShareAction,
}
