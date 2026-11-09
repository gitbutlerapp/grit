//! Clap arguments for `grit patch-id`.

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
