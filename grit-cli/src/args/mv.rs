//! Clap arguments for `grit mv`.

use clap::Args;

/// Rename or move tracked paths.
#[derive(Debug, Args)]
pub(crate) struct MvArgs {
    /// Source path(s) and final destination (last argument).
    #[arg(value_name = "PATH")]
    pub(crate) paths: Vec<String>,
    /// Replace existing files at the destination.
    #[arg(short = 'f', long = "force")]
    pub(crate) force: bool,
    /// List moves that would happen without changing the index or work tree.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
}
