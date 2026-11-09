//! Clap arguments for `grit history`.

use crate::message_input;
use clap::{Args, Subcommand};

/// Subcommands of `grit history`.
#[derive(Debug, Subcommand)]
pub(crate) enum HistoryAction {
    /// Remove a commit and replay descendants onto its parent.
    Drop {
        /// Commit to remove (revision spec).
        commit: String,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Reorder a commit on the first-parent chain of `HEAD`.
    Move {
        /// Commit to move (revision spec).
        commit: String,
        /// Place the commit immediately before this revision.
        #[arg(long = "before", conflicts_with = "after")]
        before: Option<String>,
        /// Place the commit immediately after this revision.
        #[arg(long = "after", conflicts_with = "before")]
        after: Option<String>,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Replace a commit message and replay descendants onto the new history.
    Reword {
        /// Commit to reword (revision spec).
        commit: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        #[command(flatten)]
        author: message_input::AuthorArg,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Apply staged changes onto a commit and replay descendants.
    Fixup {
        /// Commit to fix up (revision spec).
        commit: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Unstaged hunk ids to fold (`grit diff --unstaged --json`, comma-separated or repeated).
        #[arg(long = "hunks", value_name = "ID,...", action = clap::ArgAction::Append)]
        hunks: Vec<String>,
        /// Unstaged new-side line ranges to fold (`path:start-end` or `path:line`, comma-separated or repeated).
        #[arg(long = "lines", value_name = "PATH:START-END", action = clap::ArgAction::Append)]
        lines: Vec<String>,
        #[command(flatten)]
        pathspecs: crate::pathspec_args::TrailingPathspecArgs,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Assign staged hunks into recent commits and replay descendants once.
    Absorb {
        /// Oldest commit boundary (exclusive) for candidate commits.
        #[arg(long = "base")]
        base: Option<String>,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Replay local commits onto a new base (same as top-level `grit rebase`).
    Rebase {
        /// Commits to replay are those reachable from `HEAD` but not from this revision (defaults to `@{upstream}`).
        #[arg(value_name = "UPSTREAM")]
        upstream: Option<String>,
        /// Replay onto this commit instead of `UPSTREAM`.
        #[arg(long = "onto", value_name = "REV")]
        onto: Option<String>,
        /// Finish a rebase stopped on conflicts after resolving and staging paths.
        #[arg(long = "continue", conflicts_with_all = ["abort", "skip", "upstream", "onto", "check"])]
        continue_rebase: bool,
        /// Cancel an in-progress rebase and restore the branch to its pre-rebase tip.
        #[arg(long = "abort", conflicts_with_all = ["continue_rebase", "skip", "upstream", "onto", "check"])]
        abort: bool,
        /// Skip the current commit and replay the rest.
        #[arg(long = "skip", conflicts_with_all = ["continue_rebase", "abort", "upstream", "onto", "check"])]
        skip: bool,
        #[command(flatten)]
        rebase_flags: crate::commands::history::RebaseRunFlags,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Peel path-limited changes out of a commit into a new parent and replay descendants.
    Split {
        /// Commit to split (revision spec).
        commit: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Replace the remaining commit's message after the split.
        #[arg(long = "rest-message", allow_hyphen_values = true)]
        rest_message: Option<String>,
        #[command(flatten)]
        paths: crate::pathspec_args::TrailingPathspecArgs,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Undo the last applied history rewrite using the recorded ref plan.
    Undo {
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
    /// Fold a revision range into the oldest commit and replay descendants.
    Squash {
        /// Revision range (same syntax as `git rev-list`, for example `base..tip`).
        #[arg(required = true)]
        range: Vec<String>,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Build a combined autosquash-style message from every commit in the range.
        #[arg(long = "combine", conflicts_with_all = ["message", "message_file"])]
        combine: bool,
        #[command(flatten)]
        shared: crate::commands::history::HistoryRewriteArgs,
    },
}

/// Rewrite commits without a sequencer (plan, then apply refs atomically).
#[derive(Debug, Args)]
pub(crate) struct HistoryArgs {
    #[command(subcommand)]
    pub(crate) action: HistoryAction,
}
