//! Clap arguments for `grit revs`.

use crate::pathspec_args;
use crate::rev_select;
use clap::Args;

/// List or count commits matching revision ranges.
#[derive(Debug, Args)]
pub(crate) struct RevsArgs {
    #[command(flatten)]
    pub(crate) rev: rev_select::RevSelectArgs,
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::TrailingPathspecArgs,
    /// Print only the number of reachable commits.
    #[arg(long)]
    pub(crate) count: bool,
    /// Mark each commit of a symmetric range (`A...B`) as left or right; with `--count`,
    /// print left/right counts instead.
    #[arg(long = "left-right")]
    pub(crate) left_right: bool,
    /// Pair the two sides of a symmetric range (`A...B`) by patch-id and mark
    /// equivalent commits (implies `--left-right`).
    #[arg(long = "cherry-mark", conflicts_with = "count")]
    pub(crate) cherry_mark: bool,
}
