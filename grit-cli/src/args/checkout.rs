//! Clap arguments for `grit checkout`.

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

/// Start work on a repository from a snapshot of its main branch (or some of its paths) instead of a clone.
#[derive(Debug, Args)]
pub(crate) struct CheckoutArgs {
    /// The repository on a server that offers snapshot checkouts (`grit serve`).
    pub(crate) url: String,
    /// A slice (named set of paths) on the server to check out; `--path` adds to it.
    pub(crate) slice: Option<String>,
    /// Check out only this path, plus the files in the directories above it (repeatable).
    #[arg(short = 'p', long = "path", value_name = "PATH")]
    pub(crate) paths: Vec<String>,
    /// Directory to check out into (defaults to the repository name).
    #[arg(long, value_name = "DIR")]
    pub(crate) into: Option<std::path::PathBuf>,
}
