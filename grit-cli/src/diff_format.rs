//! Shared diff output format flags (`--stat`, `--name-only`, …).

use anyhow::Result;
use clap::Args;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    parse_diff_rename_threshold_percent, DiffAlgorithm, DiffDetail, DiffFilter, DiffOptions,
    RenameDetection,
};

fn parse_find_renames_threshold(s: &str) -> Result<u8, String> {
    parse_diff_rename_threshold_percent(s).ok_or_else(|| format!("invalid threshold `{s}`"))
}

/// Mutually exclusive diff output formats (default: patch for `grit diff`; none for `grit log`).
#[derive(Debug, Default, Args, Clone, Copy)]
pub struct DiffFormatArgs {
    /// Unified patch for each commit (`git log -p`).
    #[arg(
        short = 'p',
        long = "patch",
        conflicts_with_all = ["stat", "numstat", "shortstat", "name_only", "name_status"]
    )]
    pub patch: bool,
    /// Git apply-compatible patch text (`index` lines and `+`/`-` hunks), not delta-style output.
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
    /// Summary with change bars per file (like `git diff --stat`).
    #[arg(
        long = "stat",
        conflicts_with_all = ["patch", "numstat", "shortstat", "name_only", "name_status"]
    )]
    pub stat: bool,
    /// Machine-readable `insertions<TAB>deletions<TAB>path` lines.
    #[arg(
        long = "numstat",
        conflicts_with_all = ["patch", "stat", "shortstat", "name_only", "name_status"]
    )]
    pub numstat: bool,
    /// Only the summary line (`N files changed, …`).
    #[arg(
        long = "shortstat",
        conflicts_with_all = ["patch", "stat", "numstat", "name_only", "name_status"]
    )]
    pub shortstat: bool,
    /// Paths only, one per line.
    #[arg(
        long = "name-only",
        conflicts_with_all = ["patch", "stat", "numstat", "shortstat", "name_status"]
    )]
    pub name_only: bool,
    /// Status letter and path(s) per line.
    #[arg(
        long = "name-status",
        conflicts_with_all = ["patch", "stat", "numstat", "shortstat", "name_only"]
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
    /// Ignore whitespace when comparing lines (`git diff -w`).
    #[arg(short = 'w', long = "ignore-all-space")]
    pub ignore_all_space: bool,
    /// Ignore changes in the amount of whitespace (`git diff -b`).
    #[arg(short = 'b', long = "ignore-space-change")]
    pub ignore_space_change: bool,
    /// Ignore changes whose lines are all blank (`git diff --ignore-blank-lines`).
    #[arg(long = "ignore-blank-lines")]
    pub ignore_blank_lines: bool,
    /// Detect renames; optional similarity threshold (`50` default). Glued `-M30` is normalized to `-M=30` before parse.
    #[arg(
        short = 'M',
        long = "find-renames",
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "50",
        require_equals = true,
        value_parser = parse_find_renames_threshold
    )]
    find_renames: Option<Option<u8>>,
    /// Do not detect renames.
    #[arg(long = "no-renames", conflicts_with = "find_renames")]
    no_renames: bool,
    /// Line diff algorithm (overrides `diff.algorithm`).
    #[arg(long = "diff-algorithm", value_name = "ALGO")]
    diff_algorithm: Option<DiffAlgorithm>,
    /// Limit output to these change types (Git letter syntax: `ADMR…`).
    #[arg(long = "diff-filter", value_name = "LETTERS")]
    diff_filter: Option<DiffFilter>,
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
        if !self.any_format_flag() {
            return None;
        }
        Some(self.detail())
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

    /// Apply CLI flags to shared line-diff options (config is the base when flags are omitted).
    pub fn apply_to_options(
        &self,
        diff_options: &mut DiffOptions,
        config: &ConfigSet,
    ) -> Result<()> {
        if let Some(n) = self.unified_context {
            diff_options.context_lines = n;
        }
        let mut ws = diff_options.whitespace;
        ws.ignore_all_space |= self.ignore_all_space;
        ws.ignore_space_change |= self.ignore_space_change;
        ws.ignore_blank_lines |= self.ignore_blank_lines;
        diff_options.whitespace = ws;

        if self.no_renames {
            diff_options.renames = RenameDetection::Off;
        } else if let Some(threshold) = self.find_renames {
            let pct = threshold.unwrap_or(50).min(100);
            diff_options.renames = RenameDetection::Similarity(pct);
        }

        if let Some(algo) = self.diff_algorithm {
            diff_options.algorithm = algo;
        }

        if let Some(spec) = self.diff_filter {
            diff_options.filter = Some(spec);
        }

        if self.function_context {
            diff_options.function_context = true;
        }

        let _ = config;

        Ok(())
    }

    /// Apply `-U` and other shared flags (legacy name used by diff/log/show call sites).
    pub fn apply_context(self, diff_options: &mut DiffOptions) {
        let config = ConfigSet::new();
        let _ = self.apply_to_options(diff_options, &config);
    }
}

#[cfg(test)]
mod tests {
    use grit_lib::diff::parse_diff_rename_threshold_percent;

    #[test]
    fn parse_rename_threshold_accepts_git_forms() {
        assert_eq!(parse_diff_rename_threshold_percent("30"), Some(30));
        assert_eq!(parse_diff_rename_threshold_percent("30%"), Some(30));
        assert_eq!(parse_diff_rename_threshold_percent("50"), Some(50));
    }
}
