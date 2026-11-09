//! Clap arguments for `grit merge-base`.

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

/// Find merge bases or test whether one commit is an ancestor of another.
#[derive(Debug, Args)]
pub(crate) struct MergeBaseArgs {
    /// First commit or ref.
    pub(crate) a: String,
    /// Second commit or ref.
    pub(crate) b: String,
    /// List every minimal merge base (criss-cross histories).
    #[arg(long)]
    pub(crate) all: bool,
    /// Exit 0 when `a` is an ancestor of `b`, 1 otherwise; prints nothing in human mode.
    #[arg(long = "is-ancestor")]
    pub(crate) is_ancestor: bool,
}
