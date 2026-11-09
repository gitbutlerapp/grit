//! Clap arguments for `grit patch-id`.

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

/// Print stable patch-IDs for commits or revision ranges.
#[derive(Debug, Args)]
pub(crate) struct PatchIdArgs {
    /// Commits or ranges to hash; `A..B` and `A...B` expand to one ID per commit
    /// (merge commits are skipped).
    #[arg(value_name = "REV")]
    pub(crate) revs: Vec<String>,
    /// Use legacy unstable patch-ID algorithm (Git 1.9 and older).
    #[arg(long)]
    pub(crate) unstable: bool,
}
