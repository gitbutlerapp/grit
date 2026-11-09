//! Clap arguments for `grit gc`.

use clap::{Args, Subcommand};

/// Subcommands of `grit gc`.
#[derive(Debug, Subcommand)]
pub(crate) enum GcAction {
    /// Repack reachable objects into one pack.
    Repack {
        /// Write a reachability `.bitmap` sidecar for the new pack.
        #[arg(long)]
        bitmaps: bool,
        /// Recompute every delta instead of reusing the ones already in packs.
        #[arg(short, long)]
        force: bool,
        /// Report what would be packed without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Write a reachable commit-graph with changed-path Bloom filters.
    #[command(name = "commit-graph")]
    CommitGraph {
        /// Report commit count without writing the graph file.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Write or refresh the multi-pack-index.
    Midx {
        /// Report pack coverage without writing the index.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Pack loose refs into `packed-refs` (or compact reftable).
    #[command(name = "pack-refs")]
    PackRefs {
        /// Report refs that would be packed without writing.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Delete unreachable loose objects.
    Prune {
        /// Only remove objects older than this date (Git approxidate syntax).
        #[arg(long = "older-than", value_name = "DATE")]
        older_than: Option<String>,
        /// Report objects that would be deleted without removing them.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Move large blobs to a large-object promisor remote and drop them here.
    Offload {
        /// Promisor remote that receives the blobs (`remote.<name>.url`).
        #[arg(long, value_name = "REMOTE")]
        to: String,
        /// Blobs to move: `none` for every blob, or a size such as `1m` (blobs at
        /// least that big). Defaults to `remote.<REMOTE>.partialCloneFilter`.
        #[arg(long, value_name = "none|SIZE")]
        blobs: Option<String>,
        /// The promisor's git directory, when its URL is not a local path.
        #[arg(long = "lop-dir", value_name = "PATH")]
        lop_dir: Option<std::path::PathBuf>,
        /// Also set `uploadpack.allowFilter` and `uploadpack.allowAnySHA1InWant` in the promisor.
        #[arg(long = "configure-lop")]
        configure_lop: bool,
        /// Report what would move without changing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Drop large blobs a promisor remote can supply again and nothing needed recently.
    #[command(name = "drop-large-blobs")]
    DropLargeBlobs {
        /// Drop blobs at least this big, such as `1m` (default:
        /// `maintenance.drop-large-blobs.minSize`, else the promisor's `blob:limit`, else 1m).
        #[arg(long = "min-size", value_name = "SIZE")]
        min_size: Option<String>,
        /// Keep blobs needed since this date (Git approxidate; `now` keeps none for
        /// recency). Default: `maintenance.drop-large-blobs.expire`, else `2.weeks.ago`.
        #[arg(long = "older-than", value_name = "DATE")]
        older_than: Option<String>,
        /// List the blobs that would be dropped without changing anything.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}

/// Pack refs, expire reflogs, repack, prune, and refresh commit-graph.
#[derive(Debug, Args)]
pub(crate) struct GcArgs {
    /// Show what would run without writing.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Run only when `gc.auto` or `gc.autoPackLimit` is exceeded (what commands run on their own).
    #[arg(long = "auto")]
    pub(crate) auto: bool,
    /// Suppress progress summary on stderr.
    #[arg(short, long)]
    pub(crate) quiet: bool,
    #[command(subcommand)]
    pub(crate) action: Option<GcAction>,
}
