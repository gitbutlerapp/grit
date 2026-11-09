//! Clap arguments for `grit blame`.

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

/// Show who last modified each line of a file.
#[derive(Debug, Args)]
pub(crate) struct BlameArgs {
    /// Optional revision before the file (use `--` to disambiguate paths that look like refs).
    #[arg(value_name = "REV")]
    pub(crate) rev_pos: Option<String>,
    /// File to blame (repository-relative).
    #[arg(value_name = "FILE", last = true, allow_hyphen_values = true)]
    pub(crate) file: Option<String>,
    /// Blame the file as committed at this revision. Without a revision the working
    /// tree file is blamed, with uncommitted lines marked.
    #[arg(long)]
    pub(crate) rev: Option<String>,
    /// Limit output with a Git-style line range (`-L` / `--lines`).
    #[arg(
        short = 'L',
        long = "lines",
        value_name = "RANGE",
        allow_hyphen_values = true
    )]
    pub(crate) line_range: Option<String>,
    /// At merge commits, follow only the first parent (Git's `--first-parent`).
    #[arg(long = "first-parent")]
    pub(crate) first_parent: bool,
    /// Ignore this revision when attributing lines (repeatable).
    #[arg(long = "ignore-rev", value_name = "REV")]
    pub(crate) ignore_rev: Vec<String>,
    /// Read revision oids to ignore from a file (repeatable; `""` clears prior entries).
    #[arg(long = "ignore-revs-file", value_name = "FILE")]
    pub(crate) ignore_revs_file: Vec<String>,
    /// With `--json`, emit grouped `hunks[]` instead of per-line `lines[]`.
    #[arg(long)]
    pub(crate) group: bool,
    /// Ignore whitespace when comparing lines (`git blame -w`).
    #[arg(short = 'w', long = "ignore-all-space")]
    pub(crate) ignore_all_space: bool,
    /// Detect lines copied from other files (`git blame -C`; repeat up to three times).
    #[arg(short = 'C', long = "copy-detection", action = clap::ArgAction::Count)]
    pub(crate) copy_detection: u8,
    /// Detect lines moved within the same file (`git blame -M`). Enabled automatically when
    /// `-C` is used; Git treats copy detection as implying move detection.
    #[arg(short = 'M', long = "move-detection")]
    pub(crate) move_detection: bool,
    /// Blame this file's content (`-` for stdin) instead of the working tree, on top of
    /// the revision (default `HEAD`); lines not in the revision show as uncommitted.
    #[arg(long, value_name = "FILE")]
    pub(crate) contents: Option<String>,
}
