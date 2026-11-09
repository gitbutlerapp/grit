//! Clap arguments for `grit checkin`.

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

/// Send a snapshot checkout's branches back to the server, grafted onto its real history.
#[derive(Debug, Args)]
pub(crate) struct CheckinArgs {
    /// Branches to check in (default: every branch with commits on top of the snapshot).
    #[arg(value_name = "BRANCH")]
    pub(crate) branches: Vec<String>,
    /// Replace a server branch even where that drops commits it has.
    #[arg(short = 'f', long)]
    pub(crate) force: bool,
}
