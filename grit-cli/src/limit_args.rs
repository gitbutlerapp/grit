//! Shared `-n` / `--limit` argument group for list commands.

use clap::Args;

/// Maximum number of rows to return (`-n` / `--limit`).
#[derive(Debug, Args, Clone, Default)]
pub struct LimitArgs {
    /// Maximum number of rows to list.
    #[arg(
        short = 'n',
        long = "limit",
        visible_alias = "max-count",
        value_name = "N"
    )]
    pub limit: Option<usize>,
}

impl LimitArgs {
    /// Parsed limit, if any.
    #[must_use]
    pub fn get(&self) -> Option<usize> {
        self.limit
    }
}
