//! Shared diff output format flags (`--stat`, `--name-only`, …).

use clap::Args;
use grit_lib::diff::DiffDetail;
use grit_lib::diff::DiffOptions;

/// Mutually exclusive diff output formats (default: patch for `grit diff`; none for `grit log`).
#[derive(Debug, Default, Args, Clone, Copy)]
#[group(id = "diff_output_format")]
pub struct DiffFormatArgs {
    /// Unified patch for each commit (`git log -p`).
    #[arg(
        short = 'p',
        long = "patch",
        conflicts_with_all = [
            "stat",
            "numstat",
            "shortstat",
            "name_only",
            "name_status",
            "patch_text"
        ]
    )]
    pub patch: bool,
    /// Summary with change bars per file (like `git diff --stat`).
    #[arg(
        long = "stat",
        conflicts_with_all = [
            "patch",
            "numstat",
            "shortstat",
            "name_only",
            "name_status",
            "patch_text"
        ]
    )]
    pub stat: bool,
    /// Machine-readable `insertions<TAB>deletions<TAB>path` lines.
    #[arg(
        long = "numstat",
        conflicts_with_all = [
            "patch",
            "stat",
            "shortstat",
            "name_only",
            "name_status",
            "patch_text"
        ]
    )]
    pub numstat: bool,
    /// Only the summary line (`N files changed, …`).
    #[arg(
        long = "shortstat",
        conflicts_with_all = [
            "patch",
            "stat",
            "numstat",
            "name_only",
            "name_status",
            "patch_text"
        ]
    )]
    pub shortstat: bool,
    /// Paths only, one per line.
    #[arg(
        long = "name-only",
        conflicts_with_all = [
            "patch",
            "stat",
            "numstat",
            "shortstat",
            "name_status",
            "patch_text"
        ]
    )]
    pub name_only: bool,
    /// Status letter and path(s) per line.
    #[arg(
        long = "name-status",
        conflicts_with_all = ["patch", "stat", "numstat", "shortstat", "name_only", "patch_text"]
    )]
    pub name_status: bool,
    /// Git-apply-compatible unified patch (`git diff --binary` shape); not combinable with `--json`.
    #[arg(
        long = "patch-text",
        conflicts_with_all = [
            "patch",
            "stat",
            "numstat",
            "shortstat",
            "name_only",
            "name_status"
        ]
    )]
    pub patch_text: bool,
    /// Lines of context around each change (Git `-U`).
    #[arg(short = 'U', long = "unified", value_name = "N")]
    pub unified_context: Option<usize>,
    /// Expand hunks to enclosing function boundaries (Git `-W` / `--function-context`).
    #[arg(short = 'W', long = "function-context")]
    pub function_context: bool,
}

/// Per-commit diff output for [`crate::commands::shortlog`] (`--stat`, `--name-only` only).
#[derive(Debug, Default, Args, Clone, Copy)]
#[group(id = "shortlog_diff_output_format")]
pub struct ShortlogDiffFormatArgs {
    /// Summary with change bars per file (like `git log --stat`).
    #[arg(long = "stat", conflicts_with = "name_only")]
    pub stat: bool,
    /// Paths only, one per line.
    #[arg(long = "name-only", conflicts_with = "stat")]
    pub name_only: bool,
}

impl ShortlogDiffFormatArgs {
    /// Convert to the shared [`DiffFormatArgs`] shape used by commit diff loading.
    #[must_use]
    pub fn to_diff_format(self) -> DiffFormatArgs {
        DiffFormatArgs {
            stat: self.stat,
            name_only: self.name_only,
            ..DiffFormatArgs::default()
        }
    }
}

impl DiffFormatArgs {
    /// Whether any diff output format flag was set (for commands where the default is no diff).
    #[must_use]
    pub fn any_format_flag(self) -> bool {
        self.patch
            || self.patch_text
            || self.stat
            || self.numstat
            || self.shortstat
            || self.name_only
            || self.name_status
    }

    /// Selected [`DiffDetail`] when a format flag is set; `None` for plain log listing.
    #[must_use]
    pub fn detail_for_log(self) -> Option<DiffDetail> {
        if !self.any_format_flag() || self.patch_text {
            return None;
        }
        Some(self.detail())
    }

    /// Whether stdout should stream git-apply-compatible log/show patches.
    #[must_use]
    pub fn log_patch_text(self) -> bool {
        self.patch_text
    }

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

    /// Apply `-U` to shared line-diff options.
    pub fn apply_context(&self, diff_options: &mut DiffOptions) {
        if let Some(n) = self.unified_context {
            diff_options.context_lines = n;
        }
        diff_options.function_context = self.function_context;
    }
}
