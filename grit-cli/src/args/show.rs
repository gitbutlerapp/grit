//! Clap arguments for `grit show`.

use crate::diff_format;
use crate::pathspec_args;
use clap::Args;

/// Show information about a commit, tag, or branch (defaults to HEAD).
#[derive(Debug, Args)]
pub(crate) struct ShowArgs {
    /// Commits, tags, branches, or `rev:path` specs to show.
    #[arg(value_name = "OBJECT")]
    pub(crate) objects: Vec<String>,
    /// Merge commit diffs: `combined` (default) or `first-parent`.
    #[arg(long = "diff-merges", value_name = "MODE", default_value = "combined")]
    pub(crate) diff_merges: String,
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::TrailingPathspecArgs,
    #[command(flatten)]
    pub(crate) format: diff_format::DiffFormatArgs,
}
