//! Clap arguments for `grit completions`.

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
use clap_complete::Shell;

/// Generate shell completion scripts for bash, zsh, or fish.
#[derive(Debug, Args)]
pub(crate) struct CompletionsArgs {
    /// Target shell.
    pub(crate) shell: Shell,
}
