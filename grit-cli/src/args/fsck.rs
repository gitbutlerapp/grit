//! Clap arguments for `grit fsck`.

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

/// Check refs, objects, and pack indexes for problems.
#[derive(Debug, Args)]
pub(crate) struct FsckArgs {
    /// Check connectivity from refs only (skip store-wide object validation).
    #[arg(long = "connectivity-only")]
    pub(crate) connectivity_only: bool,
    /// Report unreachable objects (with `--connectivity-only`, off unless this flag is set).
    #[arg(long)]
    pub(crate) dangling: bool,
    /// Do not list dangling objects (matches Git `--no-dangling`).
    #[arg(long = "no-dangling")]
    pub(crate) no_dangling: bool,
    /// Treat ref warnings as errors.
    #[arg(long)]
    pub(crate) strict: bool,
    /// Include info-level object diagnostics in human output (matches Git `--verbose`).
    #[arg(short, long)]
    pub(crate) verbose: bool,
}
