//! Shared diff output flags (`-M`, `--diff-algorithm`, …) for `grit diff` and related commands.

use anyhow::{Context, Result};
use clap::Args;
use grit_lib::config::ConfigSet;
use grit_lib::diff::{
    parse_diff_rename_threshold_percent, DiffAlgorithm, DiffOptions, RenameDetection,
};

/// Line diff and tree diff formatting flags shared across commands.
#[derive(Debug, Default, Args, Clone)]
pub struct DiffFormatArgs {
    /// Detect renames; optional similarity threshold (`50` default, or `50%`).
    #[arg(
        short = 'M',
        long = "find-renames",
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "50"
    )]
    find_renames: Option<Option<String>>,
    /// Do not detect renames.
    #[arg(long = "no-renames", conflicts_with = "find_renames")]
    no_renames: bool,
    /// Line diff algorithm (overrides `diff.algorithm`).
    #[arg(long = "diff-algorithm", value_name = "ALGO")]
    diff_algorithm: Option<String>,
    /// Limit output to these change types (Git letter syntax: `ADMR…`).
    #[arg(long = "diff-filter", value_name = "LETTERS")]
    diff_filter: Option<String>,
}

impl DiffFormatArgs {
    /// Merge CLI flags into `DiffOptions` (config is the base when flags are omitted).
    pub fn apply_to(&self, mut options: DiffOptions, config: &ConfigSet) -> Result<DiffOptions> {
        if self.no_renames {
            options.renames = RenameDetection::Off;
        } else if let Some(threshold) = &self.find_renames {
            let token = threshold.as_deref().unwrap_or("50");
            let pct = parse_diff_rename_threshold_percent(token)
                .with_context(|| format!("invalid rename similarity threshold `{token}`"))?;
            options.renames = RenameDetection::Similarity(pct);
        }

        if let Some(algo) = &self.diff_algorithm {
            options.algorithm = algo
                .parse::<DiffAlgorithm>()
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        let _ = config;

        if let Some(spec) = &self.diff_filter {
            options.filter = Some(
                spec.parse()
                    .map_err(|e: grit_lib::diff::InvalidDiffFilter| anyhow::anyhow!("{e}"))?,
            );
        }

        Ok(options)
    }
}
