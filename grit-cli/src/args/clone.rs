//! Clap arguments for `grit clone`.

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
use clap::Args;

/// Copy a remote repository into a new directory.
///
/// Submodule gitlinks are checked out as empty directories only; grit does not fetch submodule
/// repositories or provide a `submodule` command.
#[derive(Debug, Args)]
pub(crate) struct CloneArgs {
    /// The repository to clone (URL or local path).
    pub(crate) url: String,
    /// Directory to clone into (defaults to the repository name).
    pub(crate) dir: Option<String>,
    /// Truncate fetched history to this many commits (`git clone --depth`).
    #[arg(long, value_name = "N", conflicts_with = "shallow")]
    pub(crate) depth: Option<u32>,
    /// Branch or tag to check out instead of the remote default.
    #[arg(short = 'b', long = "branch", value_name = "NAME")]
    pub(crate) branch: Option<String>,
    /// Shallow clone of the fetched ref (`git clone --depth=1`; hidden alias).
    #[arg(long, hide = true, conflicts_with = "depth")]
    pub(crate) shallow: bool,
    /// Fetch only the branch checked out (same as `git clone --single-branch`).
    #[arg(long, overrides_with = "no_single_branch")]
    pub(crate) single_branch: bool,
    /// Fetch all branch tips even when `--shallow` is set (`git clone --no-single-branch`).
    #[arg(long, overrides_with = "single_branch")]
    pub(crate) no_single_branch: bool,
    /// Do not fetch any tags from the remote.
    #[arg(long)]
    pub(crate) no_tags: bool,
    /// Name for the remote (default `origin`).
    #[arg(short = 'o', long, value_name = "NAME")]
    pub(crate) origin: Option<String>,
    /// Partial clone: omit blobs (`none`), blobs at/above a size (`1k`, `512`, …), or
    /// what the server's accepted promisor remotes hold (`auto`).
    #[arg(long, value_name = "none|auto|SIZE")]
    pub(crate) blobs: Option<String>,
    /// Set a config value in the new repository before fetching (`git clone -c`;
    /// repeatable). Unlike the global `-c`, it is kept in the clone's config.
    #[arg(long = "config", value_name = "KEY=VALUE", value_parser = global_args::parse_config_override)]
    pub(crate) config: Vec<grit_lib::config::environment::ConfigOverride>,
    /// Create a bare repository (`git clone --bare`).
    #[arg(long)]
    pub(crate) bare: bool,
    /// Do not check out the default branch (`git clone --no-checkout`).
    #[arg(long = "no-checkout")]
    pub(crate) no_checkout: bool,
    #[command(flatten)]
    pub(crate) quiet: quiet_args::QuietArgs,
}
