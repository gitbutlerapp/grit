//! Clap arguments for `grit log`.

use crate::diff_format;
use crate::pathspec_args;
use crate::rev_select;
use clap::{Args, ValueEnum};

#[derive(Clone, Copy, Debug, ValueEnum)]
pub(crate) enum LogGroupByArg {
    Author,
}

/// Show recent commits reachable from HEAD.
#[derive(Debug, Args)]
pub(crate) struct LogArgs {
    #[command(flatten)]
    pub(crate) rev: rev_select::RevSelectArgs,
    #[command(flatten)]
    pub(crate) pathspecs: pathspec_args::TrailingPathspecArgs,
    /// Continue a paged walk from the `TOKEN` a previous page printed (an empty value is ignored).
    #[arg(
            long = "cursor",
            value_name = "TOKEN",
            num_args = 0..=1,
            default_missing_value = ""
        )]
    pub(crate) cursor: Option<String>,
    #[command(flatten)]
    pub(crate) format: diff_format::DiffFormatArgs,
    /// One line per commit: short oid and subject (refs when `--decorate`).
    #[arg(long)]
    pub(crate) oneline: bool,
    /// Show branch and tag names that point at each commit.
    #[arg(long)]
    pub(crate) decorate: bool,
    /// Aggregate commits (`author` matches `git shortlog -sne` ordering).
    #[arg(long = "group-by", value_enum, value_name = "FIELD")]
    pub(crate) group_by: Option<LogGroupByArg>,
    /// With `--group-by`, omit per-commit lists in `--json` output.
    #[arg(long)]
    pub(crate) count: bool,
    /// Draw a commit graph in human output (implies topological order; Unicode by default).
    #[arg(long)]
    pub(crate) graph: bool,
    /// Write straight to the terminal instead of through the pager.
    #[arg(long = "no-pager")]
    pub(crate) no_pager: bool,
    /// Use ASCII `*|` lane characters in `--graph` output instead of Unicode box-drawing.
    #[arg(long = "graph-ascii")]
    pub(crate) graph_ascii: bool,
    /// Draw `--graph` with Unicode box-drawing (default when `--graph` is set).
    #[arg(long = "graph-unicode", conflicts_with = "graph_ascii")]
    pub(crate) graph_unicode: bool,
}
