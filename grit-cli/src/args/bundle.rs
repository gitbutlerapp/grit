//! Clap arguments for `grit bundle`.

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

/// Subcommands of `grit bundle`.
#[derive(Debug, Subcommand)]
pub(crate) enum BundleAction {
    /// Write a bundle file from revision specs.
    Create {
        /// Output bundle path.
        file: String,
        /// Commits or ref ranges to include (same rules as `grit log`).
        #[arg(value_name = "REV", conflicts_with = "all")]
        revs: Vec<String>,
        /// Include every ref tip in the bundle header (`HEAD`, all branches and tags).
        #[arg(long)]
        all: bool,
        /// Report refs, prerequisites, and object count without writing a file.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
    /// Check that a bundle file is well-formed.
    Verify {
        /// Bundle file to verify.
        file: String,
    },
    /// List refs recorded in a bundle header.
    List {
        /// Bundle file to read.
        file: String,
        #[command(flatten)]
        limit: limit_args::LimitArgs,
    },
    /// Ingest a bundle pack into the current repository.
    Unbundle {
        /// Bundle file to read.
        file: String,
        /// Verify and list ref updates without ingesting objects or writing refs.
        #[arg(long = "dry-run")]
        dry_run: bool,
    },
}


/// Create, verify, or list git bundle files.
#[derive(Debug, Args)]
pub(crate) struct BundleArgs {
    #[command(subcommand)]
    pub(crate) action: BundleAction,
}
