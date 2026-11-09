//! Clap arguments for `grit remote`.


use crate::branch_list_args;
use crate::clap_value_args;
use crate::diff_args;
use crate::diff_format;
use crate::global_args;
use crate::limit_args;
use crate::merge_strategy_args;
use crate::message_input;
use crate::pathspec_args;
use crate::quiet_args;
use crate::rev_select;
use clap::{Args, Subcommand};

/// Subcommands of `grit remote`.
#[derive(Debug, Subcommand)]
pub(crate) enum RemoteAction {
    /// Add a new remote.
    Add {
        /// Short name for the remote (e.g. origin).
        name: String,
        /// The remote's URL or path.
        url: String,
        /// Fetch from the remote after adding it.
        #[arg(long)]
        fetch: bool,
    },
    /// Set the fetch or push URL for an existing remote.
    SetUrl {
        /// Remote to update.
        name: String,
        /// New URL or path.
        url: String,
        /// Update `pushurl` instead of `url`.
        #[arg(long)]
        push: bool,
        /// Show what would change without writing config.
        #[arg(long)]
        dry_run: bool,
    },
    /// Remove a remote and its remote-tracking refs.
    Remove {
        /// Remote name to remove.
        name: String,
        /// Report changes without writing config or refs.
        #[arg(long)]
        dry_run: bool,
    },
    /// Delete stale remote-tracking refs without fetching.
    Prune {
        /// Configured remote name.
        name: String,
        /// Report refs that would be pruned without deleting them.
        #[arg(long)]
        dry_run: bool,
    },
    /// Set or delete `refs/remotes/<name>/HEAD`.
    SetHead {
        /// Configured remote name.
        name: String,
        /// Short branch name (sets HEAD to `refs/remotes/<name>/<branch>`).
        branch: Option<String>,
        /// Query the remote and set HEAD from its default branch.
        #[arg(long, conflicts_with_all = ["branch", "delete"])]
        auto: bool,
        /// Delete the local HEAD symref.
        #[arg(long, conflicts_with_all = ["branch", "auto"])]
        delete: bool,
        /// Show what would change without updating refs.
        #[arg(long)]
        dry_run: bool,
    },
    /// Summarize a remote (URLs, branches, pull config).
    Show {
        /// Configured remote name.
        name: String,
        /// Use local remote-tracking refs only; do not contact the remote.
        #[arg(long = "no-query")]
        no_query: bool,
        /// Return at most this many remote branches.
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
    },
    /// Rename an existing remote.
    Rename {
        /// Current remote name.
        old: String,
        /// New remote name.
        new: String,
        /// Show what would change without writing the repository.
        #[arg(long)]
        dry_run: bool,
    },
    /// List references on a remote (like `git ls-remote`).
    Refs {
        /// Configured remote name or a literal URL/path.
        remote_or_url: String,
        /// Only show refs under `refs/heads/`.
        #[arg(long)]
        heads: bool,
        /// Only show refs under `refs/tags/`.
        #[arg(long)]
        tags: bool,
        /// Sort refs by `name` or `version` (`git ls-remote --sort=version:refname`).
        #[arg(long = "sort", value_name = "KEY", default_value = "name")]
        sort: RemoteRefSortArg,
        /// Reverse sort order (`git ls-remote --sort=-…`).
        #[arg(long)]
        reverse: bool,
        /// Return at most this many refs after sorting.
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
        /// Optional ref prefixes (same rules as `git ls-remote`).
        #[arg(value_name = "PREFIX")]
        prefixes: Vec<String>,
    },
}

/// Sort key for `grit remote refs`.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub(crate) enum RemoteRefSortArg {
    /// Full ref name order.
    Name,
    /// Numeric `version:refname` order.
    Version,
}


/// List remotes, or add one.
#[derive(Debug, Args)]
pub(crate) struct RemoteArgs {
    /// Show fetch and push URLs (like `git remote -v`).
    #[arg(short = 'v', long)]
    pub(crate) verbose: bool,
    #[command(subcommand)]
    pub(crate) action: Option<RemoteAction>,
}
