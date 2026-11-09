//! Clap arguments for `grit add`.

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

/// Stage changes. With no paths, stages everything.
#[derive(Debug, Args)]
pub(crate) struct AddArgs {
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::PathspecArgs,
    /// Stage tracked changes only (modified and deleted paths; no untracked files).
    #[arg(short = 'u', long = "update")]
    pub(crate) update: bool,
    /// Report paths that would be staged without writing the index or object database.
    #[arg(long = "dry-run")]
    pub(crate) dry_run: bool,
    /// Record intent to add (empty index entries) instead of staging file content.
    #[arg(short = 'N', long = "intent-to-add")]
    pub(crate) intent_to_add: bool,
    /// Stage paths that would otherwise be rejected as ignored.
    #[arg(short = 'f', long = "force")]
    pub(crate) force: bool,
    /// Mark staged regular files as executable (`100755`) in the index.
    #[arg(long = "executable", conflicts_with = "non_executable")]
    pub(crate) executable: bool,
    /// Mark staged regular files as non-executable (`100644`) in the index.
    #[arg(long = "non-executable", conflicts_with = "executable")]
    pub(crate) non_executable: bool,
    /// Apply a unified diff to the index (`git apply --cached`). Use `-` for stdin.
    #[arg(long = "patch-file", value_name = "FILE|-")]
    pub(crate) patch_file: Option<String>,
    /// Stage only the listed hunk ids from `grit diff --json` (comma-separated, repeatable).
    #[arg(long = "hunks", value_name = "ID,...", action = clap::ArgAction::Append)]
    pub(crate) hunks: Vec<String>,
    /// Stage only new-side line ranges from `grit diff --json` (`path:start-end`, repeatable).
    #[arg(long = "lines", value_name = "PATH:START-END", action = clap::ArgAction::Append)]
    pub(crate) lines: Vec<String>,
    /// Unified context lines when resolving `--hunks` / `--lines` (match `grit diff -U`; default 3).
    #[arg(short = 'U', long = "unified", value_name = "N")]
    pub(crate) unified_context: Option<usize>,
}
