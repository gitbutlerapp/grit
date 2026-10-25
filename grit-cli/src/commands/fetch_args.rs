//! Shared fetch flags for `grit fetch` and `grit pull`.

use clap::Args;

/// Options shared by fetch-like commands.
#[derive(Args, Clone, Debug, Default)]
pub struct FetchArgs {
    /// Fetch from every configured remote (respects `remote.<name>.skipFetchAll`).
    #[arg(
        long = "all",
        conflicts_with = "remote",
        conflicts_with_all = ["refspecs"]
    )]
    pub all: bool,
    /// Remote to fetch from (defaults to the current branch's remote, else `origin`).
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
    #[command(flatten)]
    pub quiet: crate::quiet_args::QuietArgs,
    /// Fetch only the recent history of each branch (same as `git fetch --depth`).
    #[arg(long = "depth", value_name = "N", conflicts_with_all = ["deepen", "unshallow"])]
    pub depth: Option<u32>,
    /// Deepen an existing shallow clone by this many commits (`git fetch --deepen`).
    #[arg(long = "deepen", value_name = "N", conflicts_with_all = ["depth", "unshallow"])]
    pub deepen: Option<u32>,
    /// Convert a shallow repository to a complete one (`git fetch --unshallow`).
    #[arg(long = "unshallow", conflicts_with_all = ["depth", "deepen"])]
    pub unshallow: bool,
}

impl FetchArgs {
    /// Resolved shallow fetch control from the mutually exclusive depth flags.
    #[must_use]
    pub fn fetch_depth(&self) -> Option<grit_lib::remote::transfer::FetchDepth> {
        if self.unshallow {
            Some(grit_lib::remote::transfer::FetchDepth::Unshallow)
        } else if let Some(n) = self.deepen {
            grit_lib::remote::transfer::FetchDepth::deepen(n)
        } else if let Some(n) = self.depth {
            grit_lib::remote::transfer::FetchDepth::depth(n)
        } else {
            None
        }
    }

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
