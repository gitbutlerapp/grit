//! Clap arguments for `grit repack`.

use clap::Args;

/// Pack reachable objects and remove redundant packs.
#[derive(Debug, Args)]
pub(crate) struct RepackArgs {
    /// Recompute every delta instead of reusing the ones already in packs.
    #[arg(short, long)]
    pub(crate) force: bool,
    /// Show what would be packed without writing.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Suppress progress summary on stderr.
    #[arg(short, long)]
    pub(crate) quiet: bool,
}
