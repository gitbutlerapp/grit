//! Clap arguments for `grit restore`.

use crate::pathspec_args;
use clap::Args;

/// Restore working tree and/or index paths.
#[derive(Debug, Args)]
pub(crate) struct RestoreArgs {
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::PathspecArgs,
    /// Restore the index from HEAD (or `--source`).
    #[arg(short = 'S', long = "staged")]
    pub(crate) staged: bool,
    /// Restore the working tree (default when `--staged` is absent).
    #[arg(short = 'W', long = "worktree")]
    pub(crate) worktree: bool,
    /// Tree or commit to restore from.
    #[arg(short = 's', long = "source")]
    pub(crate) source: Option<String>,
    /// For unmerged paths, write our side (index stage 2) into the worktree.
    #[arg(long = "ours", conflicts_with_all = ["theirs", "merge", "conflict_style"])]
    pub(crate) ours: bool,
    /// For unmerged paths, write their side (index stage 3) into the worktree.
    #[arg(long = "theirs", conflicts_with_all = ["ours", "merge", "conflict_style"])]
    pub(crate) theirs: bool,
    /// Rebuild conflict markers in the worktree from index stages.
    #[arg(long = "merge", conflicts_with_all = ["ours", "theirs"])]
    pub(crate) merge: bool,
    /// Conflict marker style (implies `--merge`).
    #[arg(long = "conflict", value_enum, conflicts_with_all = ["ours", "theirs"])]
    pub(crate) conflict_style: Option<crate::conflict_style::ConflictStyleArg>,
    /// Discard only the listed unstaged hunk ids (`grit diff --unstaged --json`) or unstage staged ids (`--staged`).
    #[arg(long = "hunks", value_name = "ID,...", action = clap::ArgAction::Append)]
    pub(crate) hunks: Vec<String>,
    /// Discard or unstage only new-side line ranges (`path:start-end` from `grit diff --json`).
    #[arg(long = "lines", value_name = "PATH:START-END", action = clap::ArgAction::Append)]
    pub(crate) lines: Vec<String>,
    /// Report paths that would change without writing the worktree or index.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
}
