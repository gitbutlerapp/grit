//! Clap arguments for `grit merge-base`.

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
