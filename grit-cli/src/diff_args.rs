//! Shared diff formatting flags for porcelain commands.

use clap::Args;
use grit_lib::diff::DiffOptions;

/// Line-diff formatting flags (Git `-U` / unified context).
#[derive(Debug, Clone, Args, Default)]
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
}

impl DiffFormatArgs {
    /// Apply CLI overrides onto base [`DiffOptions`].
    #[must_use]
    pub fn to_options(&self, base: DiffOptions) -> DiffOptions {
        match self.unified {
            Some(n) => base.with_context_lines(n),
            None => base,
        }
    }
}
