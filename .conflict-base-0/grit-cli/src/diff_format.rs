//! Shared diff output format flags (`--stat`, `--name-only`, …).

use clap::Args;
use grit_lib::diff::DiffDetail;
use grit_lib::diff::DiffOptions;

/// Mutually exclusive diff output formats (default: patch).
#[derive(Debug, Default, Args, Clone, Copy)]
pub struct DiffFormatArgs {
    /// Summary with change bars per file (like `git diff --stat`).
    #[arg(
        long = "stat",
        conflicts_with_all = ["numstat", "shortstat", "name_only", "name_status"]
    )]
    pub stat: bool,
    /// Machine-readable `insertions<TAB>deletions<TAB>path` lines.
    #[arg(
        long = "numstat",
        conflicts_with_all = ["stat", "shortstat", "name_only", "name_status"]
    )]
    pub numstat: bool,
    /// Only the summary line (`N files changed, …`).
    #[arg(
        long = "shortstat",
        conflicts_with_all = ["stat", "numstat", "name_only", "name_status"]
    )]
    pub shortstat: bool,
    /// Paths only, one per line.
    #[arg(
        long = "name-only",
        conflicts_with_all = ["stat", "numstat", "shortstat", "name_status"]
    )]
    pub name_only: bool,
    /// Status letter and path(s) per line.
    #[arg(
        long = "name-status",
        conflicts_with_all = ["stat", "numstat", "shortstat", "name_only"]
    )]
    pub name_status: bool,
    /// Expand hunks to enclosing function boundaries (Git `-W`).
    #[arg(
        short = 'W',
        long = "function-context",
        conflicts_with_all = ["stat", "numstat", "shortstat", "name_only", "name_status"]
    )]
    pub function_context: bool,
    /// Lines of context around each change (Git `-U`; bare `-U` means 3).
    #[arg(
        short = 'U',
        long = "unified",
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "3"
    )]
    pub unified_context: Option<usize>,
}

impl DiffFormatArgs {
    /// Selected [`DiffDetail`] for the library.
    #[must_use]
    pub fn detail(self) -> DiffDetail {
        if self.numstat || self.stat || self.shortstat {
            DiffDetail::Stat
        } else if self.name_status {
            DiffDetail::NameStatus
        } else if self.name_only {
            DiffDetail::NameOnly
        } else {
            DiffDetail::Patch
        }
    }

    /// Whether human output should use stat-style rendering (includes `--shortstat` summary only).
    #[must_use]
    pub fn human_stat_block(self) -> bool {
        self.stat
    }

    #[must_use]
    pub fn human_numstat(self) -> bool {
        self.numstat
    }

    #[must_use]
    pub fn human_shortstat_only(self) -> bool {
        self.shortstat
    }

    #[must_use]
    pub fn human_name_only(self) -> bool {
        self.name_only
    }

    #[must_use]
    pub fn human_name_status(self) -> bool {
        self.name_status
    }

    /// Apply CLI flags to shared line-diff options.
    pub fn apply_to_options(&self, diff_options: &mut DiffOptions) {
        if let Some(n) = self.unified_context {
            diff_options.context_lines = n;
        }
        if self.function_context {
            diff_options.function_context = true;
        }
    }
}
