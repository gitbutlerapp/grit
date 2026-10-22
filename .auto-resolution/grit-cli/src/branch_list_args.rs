//! List flags for `grit branch` (ref-query filters and sort).

use clap::Args;

/// Flags that apply when listing branches (no positional `NAME`).
#[derive(Args, Debug, Default, Clone)]
pub struct BranchListArgs {
    /// Include only branches whose short name matches `GLOB` (repeatable).
    #[arg(long = "list", value_name = "GLOB", action = clap::ArgAction::Append)]
    pub patterns: Vec<String>,
    /// List remote-tracking branches under `refs/remotes/`.
    #[arg(short = 'r', long = "remotes")]
    pub remotes: bool,
    /// List local and remote-tracking branches.
    #[arg(short = 'a', long = "all")]
    pub all: bool,
    /// List branches merged into `REV` (defaults to `HEAD` when omitted).
    #[arg(
        long = "merged",
        value_name = "REV",
        num_args = 0..=1,
        default_missing_value = "HEAD"
    )]
    pub merged: Option<String>,
    /// List branches not merged into `REV` (defaults to `HEAD` when omitted).
    #[arg(
        long = "no-merged",
        value_name = "REV",
        num_args = 0..=1,
        default_missing_value = "HEAD"
    )]
    pub no_merged: Option<String>,
    /// List branches whose tip contains `REV` (defaults to `HEAD` when omitted).
    #[arg(
        long = "contains",
        value_name = "REV",
        num_args = 0..=1,
        default_missing_value = "HEAD"
    )]
    pub contains: Option<String>,
    /// Sort key: `name`, `version`, or `date`.
    #[arg(long = "sort", value_name = "KEY")]
    pub sort: Option<String>,
    /// Reverse sort order.
    #[arg(long = "reverse")]
    pub reverse: bool,
    /// Maximum number of branches to print.
    #[arg(short = 'n', long = "max-count", value_name = "N")]
    pub limit: Option<usize>,
    /// Print the current branch name only (same as `git branch --show-current`).
    #[arg(long = "show-current")]
    pub show_current: bool,
}
