//! Shared diff formatting flags for porcelain commands.

use clap::Args;
use grit_lib::diff::DiffOptions;

/// Line-diff formatting flags (Git `-U` / unified context).
#[derive(Debug, Clone, Args)]
pub struct DiffFormatArgs {
    /// Lines of unchanged context around each change (Git `-U`).
    #[arg(
        short = 'U',
        long = "unified",
        value_name = "LINES",
        default_value_t = 3
    )]
    pub unified: usize,
}

impl DiffFormatArgs {
    /// Apply CLI overrides onto repository-configured [`DiffOptions`].
    #[must_use]
    pub fn to_options(&self, base: DiffOptions) -> DiffOptions {
        base.with_context_lines(self.unified)
    }
}
