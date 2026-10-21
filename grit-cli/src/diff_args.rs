//! Shared diff formatting flags for porcelain commands.

use anyhow::{Context, Result};
use clap::Args;
use grit_lib::diff::{
    parse_diff_rename_threshold_percent, DiffAlgorithm, DiffOptions, RenameDetection,
};

/// Line-diff and tree-diff formatting flags (Git `-U` / unified context, `-M`, filters).
#[derive(Debug, Clone, Default, Args)]
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
    /// Detect renames; optional similarity threshold (`50` default, or `50%`). Use `-M`, `-M30`,
    /// or `--find-renames` / `--find-renames=30` (long form requires `=` when a threshold is given).
    #[arg(
        short = 'M',
        long = "find-renames",
        value_name = "N",
        num_args = 0..=1,
        default_missing_value = "50",
        require_equals = true
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
    pub fn to_options(&self, base: DiffOptions) -> Result<DiffOptions> {
        let mut options = match self.unified {
            Some(n) => base.with_context_lines(n),
            None => base,
        };
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

        if let Some(spec) = &self.diff_filter {
            options.filter = Some(
                spec.parse()
                    .map_err(|e: grit_lib::diff::InvalidDiffFilter| anyhow::anyhow!("{e}"))?,
            );
        }

        let mut ws = options.whitespace;
        ws.ignore_all_space |= self.ignore_all_space;
        ws.ignore_space_change |= self.ignore_space_change;
        ws.ignore_blank_lines |= self.ignore_blank_lines;
        options.whitespace = ws;

        Ok(options)
    }
}

/// Rewrite argv tokens so Git-style attached `-M` thresholds parse under clap (`-M30`, `-M100%`).
#[must_use]
pub fn normalize_find_renames_argv<I, S>(args: I) -> Vec<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    let mut out = Vec::new();
    for arg in args {
        let s = arg.as_ref();
        if let Some(rest) = s.strip_prefix("-M") {
            if rest.is_empty() {
                out.push("-M".to_owned());
            } else if rest.starts_with('=') {
                out.push(s.to_owned());
            } else {
                out.push(format!("-M={rest}"));
            }
        } else {
            out.push(s.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_attached_m_threshold() {
        assert_eq!(
            normalize_find_renames_argv(["diff", "-M30", "HEAD~1"]),
            ["diff", "-M=30", "HEAD~1"]
        );
        assert_eq!(normalize_find_renames_argv(["-M100%"]), ["-M=100%"]);
    }
}
