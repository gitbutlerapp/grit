//! Clap arguments for `grit reflog`.

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

/// Subcommands of `grit reflog`.
#[derive(Debug, Subcommand)]
pub(crate) enum ReflogAction {
    /// Remove one or more reflog entries (`ref@{n}`).
    Delete {
        /// Reflog selectors (`main@{1}`, `HEAD@{0}`, …).
        #[arg(required = true, value_name = "REF@{N}")]
        specs: Vec<String>,
    },
    /// Drop reflog entries older than configured or explicit cutoffs.
    Expire {
        /// Process every ref under `refs/`, not only refs that already have log files.
        #[arg(long)]
        all: bool,
        /// Expire reachable entries older than this (Git `--expire` syntax).
        #[arg(long = "expire", value_name = "DATE")]
        expire: Option<String>,
        /// Expire unreachable entries older than this.
        #[arg(long = "expire-unreachable", value_name = "DATE")]
        expire_unreachable: Option<String>,
        /// Report what would be pruned without rewriting reflog files.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// List refs that currently have a reflog.
    List {
        /// Return at most this many ref names (sorted).
        #[arg(short = 'n', long = "limit", value_name = "N")]
        limit: Option<usize>,
    },
}


/// Show, delete, or expire reflog entries.
#[derive(Debug, Args)]
pub(crate) struct ReflogArgs {
    /// Ref to show (defaults to `HEAD`).
    #[arg(value_name = "REF")]
    pub(crate) reference: Option<String>,
    /// Show at most this many entries (newest first).
    #[arg(short = 'n', long = "max-count", alias = "limit", value_name = "N")]
    pub(crate) max_count: Option<usize>,
    /// Exclude entries older than this date (Git `--since`).
    #[arg(long = "since", value_name = "DATE")]
    pub(crate) since: Option<String>,
    /// Alias for `--since`.
    #[arg(long = "after", value_name = "DATE")]
    pub(crate) after: Option<String>,
    /// Exclude entries newer than this date (Git `--until`).
    #[arg(long = "until", value_name = "DATE")]
    pub(crate) until: Option<String>,
    /// Alias for `--until`.
    #[arg(long = "before", value_name = "DATE")]
    pub(crate) before: Option<String>,
    /// Keep entries whose message contains this substring (case-sensitive).
    #[arg(long = "grep", value_name = "PAT")]
    pub(crate) grep: Option<String>,
    #[command(subcommand)]
    pub(crate) action: Option<ReflogAction>,
}
