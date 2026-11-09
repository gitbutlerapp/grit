//! Clap arguments for `grit ignore`.

use clap::{Args, Subcommand};

/// Subcommands of `grit ignore`.
#[derive(Debug, Subcommand)]
pub(crate) enum IgnoreAction {
    /// Print whether paths match exclude rules.
    Check {
        /// Paths to query (repository-relative or from the current directory).
        #[arg(value_name = "PATH")]
        paths: Vec<String>,
        /// Also list paths that are not ignored.
        #[arg(long)]
        all: bool,
    },
}

/// Query exclude (gitignore) rules.
#[derive(Debug, Args)]
pub(crate) struct IgnoreArgs {
    #[command(subcommand)]
    pub(crate) action: IgnoreAction,
}
