//! Clap arguments for `grit completions`.

use clap::Args;
use clap_complete::Shell;

/// Generate shell completion scripts for bash, zsh, or fish.
#[derive(Debug, Args)]
pub(crate) struct CompletionsArgs {
    /// Target shell.
    pub(crate) shell: Shell,
}
