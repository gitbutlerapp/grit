//! Clap arguments for `grit clean`.

use crate::pathspec_args;
use clap::Args;

/// Remove untracked files from the working tree.
#[derive(Debug, Args)]
pub(crate) struct CleanArgs {
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::PathspecArgs,
    /// Actually delete files. Without this flag, only prints what would be removed.
    #[arg(short, long, conflicts_with = "dry_run")]
    pub(crate) force: bool,
    /// Explicit dry run (default when `-f` is not passed).
    #[arg(long = "dry-run", conflicts_with = "force")]
    pub(crate) dry_run: bool,
    /// Remove ignored files as well (like `git clean -x`).
    #[arg(long, conflicts_with = "only_ignored")]
    pub(crate) ignored: bool,
    /// Remove only ignored files (like `git clean -X`).
    #[arg(long = "only-ignored", conflicts_with = "ignored")]
    pub(crate) only_ignored: bool,
    /// Additional exclude patterns; matching paths are not removed.
    #[arg(short = 'e', long = "exclude", value_name = "PATTERN", action = clap::ArgAction::Append)]
    pub(crate) exclude: Vec<String>,
    /// Remove only files, not untracked directories.
    #[arg(long = "files-only")]
    pub(crate) files_only: bool,
}
