//! Clap arguments for `grit hooks`.

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
use clap::{Args, Subcommand};

/// Subcommands of `grit hooks`.
#[derive(Debug, Subcommand)]
pub(crate) enum HooksAction {
    /// List hook scripts in the resolved hooks directory.
    List {
        #[command(flatten)]
        limit: limit_args::LimitArgs,
    },
    /// Run a hook by name (passes `--` arguments through to the script).
    Run {
        /// Hook basename (e.g. `pre-commit`).
        name: String,
        /// Arguments forwarded to the hook after `--`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        hook_args: Vec<String>,
    },
}


/// List hook scripts or run one by name.
#[derive(Debug, Args)]
pub(crate) struct HooksArgs {
    #[command(flatten)]
    pub(crate) limit: limit_args::LimitArgs,
    #[command(subcommand)]
    pub(crate) action: Option<HooksAction>,
}
