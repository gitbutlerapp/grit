//! Shared fetch flags for `grit fetch` and (later) `grit pull`.

use clap::Args;

/// Options shared by fetch-like commands.
#[derive(Args, Clone, Debug, Default)]
pub struct FetchArgs {
    /// Remote to fetch from when no refspec names one (defaults to `origin`).
    #[arg(value_name = "REMOTE")]
    pub remote: Option<String>,
    /// Refspecs selecting refs to fetch (`main`, `+refs/heads/x:refs/heads/y`, …).
    #[arg(value_name = "REFSPEC", num_args = 0..)]
    pub refspecs: Vec<String>,
    /// Fetch all tags from the remote (`git fetch --tags`).
    #[arg(long = "tags")]
    pub tags: bool,
    /// Do not fetch tags automatically (`git fetch --no-tags`).
    #[arg(long = "no-tags", conflicts_with = "tags")]
    pub no_tags: bool,
    /// Remove remote-tracking refs that no longer exist on the remote.
    #[arg(long = "prune")]
    pub prune: bool,
    /// Do not prune stale remote-tracking refs, even when config enables it.
    #[arg(long = "no-prune", conflicts_with = "prune")]
    pub no_prune: bool,
    /// Report ref updates without writing refs or objects.
    #[arg(long = "dry-run")]
    pub dry_run: bool,
    /// Suppress progress and human output.
    #[arg(short = 'q', long = "quiet")]
    pub quiet: bool,
}

impl FetchArgs {
    /// CLI override for tag mode: `Some(true)` → all tags, `Some(false)` → none.
    #[must_use]
    pub fn tag_mode_override(&self) -> Option<bool> {
        if self.tags {
            Some(true)
        } else if self.no_tags {
            Some(false)
        } else {
            None
        }
    }

    /// CLI override for prune, when `--prune` / `--no-prune` was passed.
    #[must_use]
    pub fn prune_override(&self) -> Option<bool> {
        if self.prune {
            Some(true)
        } else if self.no_prune {
            Some(false)
        } else {
            None
        }
    }
}
