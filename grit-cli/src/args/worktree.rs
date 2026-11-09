//! Clap arguments for `grit worktree`.

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

/// Subcommands of `grit worktree`.
#[derive(Debug, Subcommand)]
pub(crate) enum WorktreeAction {
    /// List registered worktrees (main first, then linked).
    List,
    /// Create a linked worktree at `path`.
    Add {
        /// Directory for the new working tree.
        path: std::path::PathBuf,
        /// Commit or branch to check out (defaults to `HEAD`).
        #[arg(value_name = "REV")]
        start_point: Option<String>,
        /// Create and check out a new branch.
        #[arg(short = 'b', long = "branch", value_name = "NAME", conflicts_with_all = ["force_branch", "detach"])]
        branch: Option<String>,
        /// Create or reset a branch to the start point (`-B`).
        #[arg(short = 'B', value_name = "NAME", conflicts_with_all = ["branch", "detach"])]
        force_branch: Option<String>,
        /// Check out in detached HEAD mode.
        #[arg(long = "detach", conflicts_with_all = ["branch", "force_branch"])]
        detach: bool,
        /// Reuse a stale registration when the working tree directory is missing.
        #[arg(short, long)]
        force: bool,
        /// Mark the new worktree locked before checkout finishes.
        #[arg(long = "lock")]
        lock: bool,
        /// Reason stored in the lock file (requires `--lock`).
        #[arg(long = "reason", requires = "lock")]
        reason: Option<String>,
        /// Register the worktree without populating files (HEAD only).
        #[arg(long = "no-checkout")]
        no_checkout: bool,
        /// Set upstream when the start point is a remote-tracking branch.
        #[arg(long = "track")]
        track: bool,
    },
    /// Remove a linked worktree.
    Remove {
        /// Path to the worktree to remove.
        path: std::path::PathBuf,
        /// Remove even when the worktree has local changes or is locked.
        #[arg(short, long)]
        force: bool,
    },
    /// Drop registrations for linked worktrees whose directories are gone.
    Prune {
        /// List prunable worktrees without removing registrations.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Mark a linked worktree as locked (prevents accidental removal).
    Lock {
        /// Path to the linked worktree.
        path: std::path::PathBuf,
        /// Optional reason stored in the lock file.
        #[arg(long)]
        reason: Option<String>,
    },
    /// Clear the lock on a linked worktree.
    Unlock {
        /// Path to the linked worktree.
        path: std::path::PathBuf,
    },
    /// Move a linked worktree to a new directory.
    Move {
        /// Current worktree path.
        worktree: std::path::PathBuf,
        /// New path for the working tree.
        new_path: std::path::PathBuf,
        /// Allow move when the worktree is locked.
        #[arg(short, long)]
        force: bool,
        /// Preview the move without changing disk state.
        #[arg(long)]
        dry_run: bool,
    },
    /// Fix `.git` / `gitdir` links after a manual directory move.
    Repair {
        /// Linked worktree roots to repair (default: all registered linked worktrees).
        #[arg(value_name = "PATH")]
        paths: Vec<std::path::PathBuf>,
        /// Report repairs without writing files.
        #[arg(long)]
        dry_run: bool,
    },
}


/// List, add, or remove linked worktrees.
#[derive(Debug, Args)]
pub(crate) struct WorktreeArgs {
    #[command(subcommand)]
    pub(crate) action: WorktreeAction,
}
