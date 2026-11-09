//! Clap arguments for `grit resolve`.

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

/// Resolve revision specs to object ids and types.
#[derive(Debug, Args)]
pub(crate) struct ResolveArgs {
    /// Revision specs to resolve (same forms as history commands).
    #[arg(
            value_name = "SPEC",
            required_unless_present = "verify",
            num_args = 1..,
            id = "specs",
            conflicts_with = "verify"
        )]
    pub(crate) specs: Vec<String>,
    /// Print abbreviated object ids.
    #[arg(long)]
    pub(crate) short: bool,
    /// Print the shortest unambiguous ref name for each spec (like `git rev-parse --abbrev-ref`).
    #[arg(long = "abbrev-ref", conflicts_with = "symbolic_full_name")]
    pub(crate) abbrev_ref: bool,
    /// Print the full symbolic ref name for each spec (like `git rev-parse --symbolic-full-name`).
    #[arg(long = "symbolic-full-name", conflicts_with = "abbrev_ref")]
    pub(crate) symbolic_full_name: bool,
    /// Resolve exactly one spec strictly (`git rev-parse --verify`); pass the spec as the flag value.
    #[arg(
            long = "verify",
            value_name = "SPEC",
            num_args = 1,
            id = "verify",
            conflicts_with_all = ["abbrev_ref", "symbolic_full_name", "specs"]
        )]
    pub(crate) verify_spec: Option<String>,
    /// With `--verify`, answer yes/no via exit status only (no stdout/stderr).
    #[arg(short = 'q', long = "quiet", requires = "verify")]
    pub(crate) quiet: bool,
}
