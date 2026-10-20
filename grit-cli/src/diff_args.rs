//! Shared diff formatting flags for porcelain commands.

use clap::Args;
use grit_lib::diff::DiffOptions;

/// Line-diff formatting flags (Git `-U` / unified context).
#[derive(Debug, Clone, Args)]
pub struct DiffFormatArgs {
    /// Lines of unchanged context around each change (Git `-U`). When omitted, uses
    /// `diff.context` from config (default 3).
    #[arg(
        short = 'U',
        long = "unified",
        value_name = "LINES",
        num_args = 0..=1,
        default_missing_value = "3"
    )]
    pub unified: Option<usize>,
    /// Ignore whitespace when comparing lines (`git diff -w`).
    #[arg(short = 'w', long = "ignore-all-space")]
    pub ignore_all_space: bool,
    /// Ignore changes in the amount of whitespace (`git diff -b`).
    #[arg(short = 'b', long = "ignore-space-change")]
    pub ignore_space_change: bool,
    /// Ignore changes whose lines are all blank (`git diff --ignore-blank-lines`).
    #[arg(long = "ignore-blank-lines")]
    pub ignore_blank_lines: bool,
}

impl DiffFormatArgs {
    /// Apply CLI overrides onto repository-configured [`DiffOptions`].
    #[must_use]
    pub fn to_options(&self, base: DiffOptions) -> DiffOptions {
        let mut opts = match self.unified {
            Some(n) => base.with_context_lines(n),
            None => base,
        };
        let mut ws = opts.whitespace;
        ws.ignore_all_space |= self.ignore_all_space;
        ws.ignore_space_change |= self.ignore_space_change;
        ws.ignore_blank_lines |= self.ignore_blank_lines;
        opts.whitespace = ws;
        opts
    }
}
