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
    #[allow(dead_code)]
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

/// Rewrite Git-style attached `-M` thresholds so clap can parse them (`-M30` → `-M=30`).
///
/// Only applies to the `diff` subcommand, after global flags and before `--`.
#[must_use]
pub fn normalize_diff_argv(mut args: Vec<String>) -> Vec<String> {
    let Some(diff_idx) = locate_diff_subcommand(&args) else {
        return args;
    };
    let mut end = args.len();
    for (i, arg) in args.iter().enumerate().skip(diff_idx + 1) {
        if arg == "--" {
            end = i;
            break;
        }
    }
    for arg in args.iter_mut().take(end).skip(diff_idx + 1) {
        if let Some(replacement) = rewrite_attached_find_renames(arg) {
            *arg = replacement;
        }
    }
    args
}

fn locate_diff_subcommand(args: &[String]) -> Option<usize> {
    let mut i = 1;
    while i < args.len() {
        let token = args[i].as_str();
        if is_leading_global_flag(token) {
            i = skip_global_flag(args, i);
            continue;
        }
        if matches!(token, "diff" | "show" | "log") {
            return Some(i);
        }
        return None;
    }
    None
}

fn is_leading_global_flag(token: &str) -> bool {
    matches!(
        token,
        "--json"
            | "--markdown"
            | "--filter"
            | "--threads"
            | "-v"
            | "-V"
            | "--version"
            | "-h"
            | "--help"
    ) || token.starts_with("--filter=")
        || token.starts_with("--threads=")
}

fn skip_global_flag(args: &[String], i: usize) -> usize {
    let token = args[i].as_str();
    if (token == "--filter" || token == "--threads") && i + 1 < args.len() {
        return i + 2;
    }
    i + 1
}

fn rewrite_attached_find_renames(token: &str) -> Option<String> {
    if token == "-M" || token.starts_with("-M=") || token.starts_with("--find-renames") {
        return None;
    }
    let rest = token.strip_prefix("-M")?;
    if rest.is_empty() || rest.starts_with('=') {
        return None;
    }
    if !attached_rename_threshold_token(rest) {
        return None;
    }
    Some(format!("-M={rest}"))
}

fn attached_rename_threshold_token(rest: &str) -> bool {
    let mut saw_digit = false;
    for ch in rest.chars() {
        if ch == '%' {
            return saw_digit;
        }
        if ch.is_ascii_digit() {
            saw_digit = true;
            continue;
        }
        return false;
    }
    saw_digit
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_attached_m_threshold_after_diff() {
        let args = vec![
            "grit".into(),
            "diff".into(),
            "-M30".into(),
            "HEAD~1".into(),
            "HEAD".into(),
        ];
        let out = normalize_diff_argv(args);
        assert_eq!(out[2], "-M=30");
        let pct = normalize_diff_argv(vec![
            "grit".into(),
            "diff".into(),
            "-M100%".into(),
            "HEAD".into(),
        ]);
        assert_eq!(pct[2], "-M=100%");
        let bare = normalize_diff_argv(vec![
            "grit".into(),
            "diff".into(),
            "-M5".into(),
            "HEAD".into(),
        ]);
        assert_eq!(bare[2], "-M=5");
    }

    #[test]
    fn normalize_attached_m_with_global_json_before_diff() {
        let args = vec![
            "grit".into(),
            "--json".into(),
            "diff".into(),
            "-M30".into(),
            "HEAD~1".into(),
            "HEAD".into(),
        ];
        let out = normalize_diff_argv(args);
        assert_eq!(out[3], "-M=30");
    }

    #[test]
    fn normalize_skips_pathspecs_after_double_dash() {
        let args = vec![
            "grit".into(),
            "diff".into(),
            "HEAD~1".into(),
            "HEAD".into(),
            "--".into(),
            "-M30".into(),
        ];
        let out = normalize_diff_argv(args);
        assert_eq!(out.last().map(String::as_str), Some("-M30"));
    }

    #[test]
    fn normalize_global_filter_with_value_before_diff() {
        let args = vec![
            "grit".into(),
            "--filter".into(),
            ".files".into(),
            "diff".into(),
            "-M5".into(),
            "HEAD".into(),
        ];
        let out = normalize_diff_argv(args);
        assert_eq!(out[4], "-M=5");
    }

    #[test]
    fn normalize_global_threads_with_value_before_diff() {
        let args = vec![
            "grit".into(),
            "--threads".into(),
            "4".into(),
            "diff".into(),
            "-M5".into(),
            "HEAD".into(),
        ];
        let out = normalize_diff_argv(args);
        assert_eq!(out[4], "-M=5");
    }
}
