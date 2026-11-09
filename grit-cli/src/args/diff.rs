//! Clap arguments for `grit diff`.

use crate::diff_args;
use crate::diff_format;
use crate::pathspec_args;
use clap::Args;

/// Show changes as a diff. No argument: uncommitted changes; with a commit:
/// the change that commit introduced; with two commits (or `A..B`) diff their trees.
#[derive(Debug, Args)]
pub(crate) struct DiffArgs {
    /// Compare two filesystem paths outside any repository (like `git diff --no-index`).
    #[arg(long = "no-index")]
    pub(crate) no_index: bool,
    /// Staged changes only (index vs HEAD).
    #[arg(short = 'S', long = "staged", visible_aliases = ["cached"])]
    pub(crate) staged: bool,
    /// Unstaged changes only (worktree vs index).
    #[arg(long = "unstaged")]
    pub(crate) unstaged: bool,
    /// Compare the worktree to this ref's tree instead of HEAD (no commit arguments).
    #[arg(long = "against", value_name = "REF")]
    pub(crate) against: Option<String>,
    /// Exit with 1 when there are changes (errors exit 2).
    #[arg(long = "exit-code")]
    pub(crate) exit_code: bool,
    /// Print nothing; implies `--exit-code`.
    #[arg(long = "quiet")]
    pub(crate) quiet: bool,
    /// One commit (that commit's patch), two commits (tree diff), or a range `A..B` / `A...B`.
    #[arg(value_name = "COMMIT")]
    pub(crate) commits: Vec<String>,
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::TrailingPathspecArgs,
    #[command(flatten)]
    pub(crate) format: diff_format::DiffFormatArgs,
    #[command(flatten)]
    pub(crate) line_format: diff_args::DiffCompareArgs,
}
