//! Clap arguments for `grit notes`.

use crate::limit_args;
use crate::message_input;
use clap::{Args, Subcommand};

/// Subcommands of `grit notes`.
#[derive(Debug, Subcommand)]
pub(crate) enum NotesAction {
    /// List notes attached to commits.
    List {
        #[command(flatten)]
        limit: limit_args::LimitArgs,
    },
    /// Show the note for a revision.
    Show {
        /// Commit (or other object) to show the note for.
        rev: String,
    },
    /// Add or replace a note on a revision.
    Add {
        /// Revision to attach the note to.
        rev: String,
        #[command(flatten)]
        message: message_input::MessageInputArgs,
        /// Replace an existing note.
        #[arg(long)]
        force: bool,
    },
    /// Remove notes from one or more revisions.
    Remove {
        /// Revisions whose notes should be removed.
        #[arg(value_name = "REV")]
        revs: Vec<String>,
    },
}

/// List, show, add, or remove Git notes on commits.
#[derive(Debug, Args)]
pub(crate) struct NotesArgs {
    /// Notes ref (default: `core.notesRef` or `refs/notes/commits`).
    #[arg(long = "ref", value_name = "REF", global = true)]
    pub(crate) notes_ref: Option<String>,
    /// Preview `add` or `remove` without writing objects or refs.
    #[arg(long = "dry-run", global = true)]
    pub(crate) dry_run: bool,
    #[command(flatten)]
    pub(crate) limit: limit_args::LimitArgs,
    #[command(subcommand)]
    pub(crate) action: Option<NotesAction>,
}
