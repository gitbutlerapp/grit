//! Clap arguments for `grit integrate`.

use clap::Args;

/// Merge a branch into the server's target branch (checking it in first), then refresh this checkout.
#[derive(Debug, Args)]
pub(crate) struct IntegrateArgs {
    /// The branch to merge.
    #[arg(value_name = "BRANCH")]
    pub(crate) branch: String,
    /// For a shared branch: integrate without waiting for this member's
    /// changes, which stay in their copy. Repeatable.
    #[arg(long, value_name = "USER")]
    pub(crate) without: Vec<String>,
}
