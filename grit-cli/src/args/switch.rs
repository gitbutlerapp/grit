//! Clap arguments for `grit switch`.

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

/// Switch to another branch, remote branch, or detached commit.
#[derive(Debug, Args)]
pub(crate) struct SwitchArgs {
    /// Branch to switch to, revision with `--detach`, or new branch name with `-c`.
    #[arg(value_name = "NAME", required_unless_present = "detach")]
    pub(crate) branch: Option<String>,
    /// With `-c`, the commit to start the branch from (defaults to `HEAD`).
    pub(crate) start_point: Option<String>,
    /// Create the branch first, then switch to it.
    #[arg(short = 'c', long = "create")]
    pub(crate) create: bool,
    /// Configure upstream from the start point when creating with `-c`.
    #[arg(
            short = 't',
            long = "track",
            conflicts_with = "no_track",
            requires = "create",
            conflicts_with_all = ["detach"]
        )]
    pub(crate) track: bool,
    /// Do not configure upstream when creating with `-c`.
    #[arg(
            long = "no-track",
            conflicts_with = "track",
            requires = "create",
            conflicts_with_all = ["detach"]
        )]
    pub(crate) no_track: bool,
    /// Check out a commit in detached HEAD mode.
    #[arg(long = "detach")]
    pub(crate) detach: bool,
    /// Run overwrite checks without updating refs, the index, or the worktree.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
}
