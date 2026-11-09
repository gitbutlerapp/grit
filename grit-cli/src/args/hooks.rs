//! Clap arguments for `grit hooks`.

use crate::limit_args;
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
