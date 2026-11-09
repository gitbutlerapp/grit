//! Clap arguments for `grit stash`.

use clap::{Args, Subcommand};

/// Subcommands of `grit stash`.
#[derive(Debug, Subcommand)]
pub(crate) enum StashAction {
    /// Save local changes to the stash and reset the working tree.
    Push {
        /// Stash message.
        #[arg(short = 'm', long = "message", allow_hyphen_values = true)]
        message: Option<String>,
        /// Include untracked files in the stash.
        #[arg(
            short = 'u',
            long = "include-untracked",
            conflicts_with_all = ["hunks", "lines"]
        )]
        include_untracked: bool,
        /// Stash only unstaged hunks from `grit diff --json`.
        #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            conflicts_with = "include_untracked"
        )]
        hunks: Vec<String>,
        /// Stash only unstaged line ranges (`path:start-end`).
        #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            conflicts_with = "include_untracked"
        )]
        lines: Vec<String>,
        /// Preview stash without updating refs, index, or worktree.
        #[arg(long = "dry-run")]
        dry_run: bool,
        /// Stash only index changes vs `HEAD`.
        #[arg(
            short = 'S',
            long = "staged",
            conflicts_with_all = ["keep_index", "hunks", "lines", "include_untracked"]
        )]
        staged: bool,
        /// Leave staged content in the index and worktree after stashing.
        #[arg(
            short = 'k',
            long = "keep-index",
            conflicts_with_all = ["staged", "hunks", "lines"]
        )]
        keep_index: bool,
        #[command(flatten)]
        pathspecs: crate::pathspec_args::TrailingPathspecArgs,
    },
    /// List stash entries.
    List,
    /// Show the summary or patch for a stash entry.
    Show {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Show a diffstat summary (default).
        #[arg(long = "stat")]
        stat: bool,
        /// Show the full patch instead of a diffstat.
        #[arg(short = 'p', long = "patch")]
        patch: bool,
        /// Include untracked files from the stash U parent.
        #[arg(short = 'u', long = "include-untracked")]
        include_untracked: bool,
    },
    /// Create a branch at the stash base commit and apply the stash with `--index`.
    Branch {
        /// New branch name.
        name: String,
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
    /// Remove every stash entry.
    Clear {
        /// Delete all stash entries (required).
        #[arg(long = "force")]
        force: bool,
        /// Report how many entries would be cleared without deleting.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Apply a stash entry without removing it.
    Apply {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Restore the stashed index as well as the worktree.
        #[arg(long = "index")]
        restore_index: bool,
    },
    /// Apply a stash entry and remove it when there are no conflicts.
    Pop {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
        /// Restore the stashed index as well as the worktree.
        #[arg(long = "index")]
        restore_index: bool,
    },
    /// Remove a stash entry without applying it.
    Drop {
        /// Stash index (`0` is newest) or `stash@{n}`.
        index: Option<String>,
    },
}

/// Save or restore shelved work.
#[derive(Debug, Args)]
pub(crate) struct StashArgs {
    /// Stash message (bare `grit stash` / `grit stash push`).
    #[arg(
        short = 'm',
        long = "message",
        global = true,
        allow_hyphen_values = true
    )]
    pub(crate) message: Option<String>,
    /// Include untracked files (bare `grit stash` / `grit stash push`).
    #[arg(
            short = 'u',
            long = "include-untracked",
            global = true,
            conflicts_with_all = ["hunks", "lines"]
        )]
    pub(crate) include_untracked: bool,
    /// Stash only unstaged hunks from `grit diff --json` (comma-separated, repeatable).
    #[arg(
            long = "hunks",
            value_name = "ID,...",
            action = clap::ArgAction::Append,
            global = true,
            conflicts_with = "include_untracked"
        )]
    pub(crate) hunks: Vec<String>,
    /// Stash only unstaged line ranges (`path:start-end`, same as `grit add --lines`).
    #[arg(
            long = "lines",
            value_name = "PATH:START-END",
            action = clap::ArgAction::Append,
            global = true,
            conflicts_with = "include_untracked"
        )]
    pub(crate) lines: Vec<String>,
    /// Preview stash without updating refs, index, or worktree.
    #[arg(long = "dry-run", global = true)]
    pub(crate) dry_run: bool,
    /// Stash only index changes vs `HEAD` (unstaged edits on other paths remain).
    #[arg(
            short = 'S',
            long = "staged",
            global = true,
            conflicts_with_all = ["keep_index", "hunks", "lines", "include_untracked"]
        )]
    pub(crate) staged: bool,
    /// After stashing, leave the index and worktree matching staged content.
    #[arg(
            short = 'k',
            long = "keep-index",
            global = true,
            conflicts_with_all = ["staged", "hunks", "lines"]
        )]
    pub(crate) keep_index: bool,
    /// Cap path lists in `--dry-run` preview output.
    #[arg(short = 'n', long = "limit", global = true, value_name = "N")]
    pub(crate) stash_limit: Option<usize>,
    #[command(flatten)]
    pub(crate) pathspecs: crate::pathspec_args::TrailingPathspecArgs,
    #[command(subcommand)]
    pub(crate) action: Option<StashAction>,
}
