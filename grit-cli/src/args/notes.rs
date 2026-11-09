//! Clap arguments for `grit notes`.

use super::NotesAction;
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
