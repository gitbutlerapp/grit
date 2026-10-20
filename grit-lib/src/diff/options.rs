//! Shared line-diff options for structured hunks, log patches, and tree diffs.

use crate::config::ConfigSet;

use super::filter::DiffFilter;
use super::{indent_heuristic_from_config, resolve_indent_heuristic};

/// Line diff algorithm names matching Git's `diff.algorithm` and `--diff-algorithm`.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffAlgorithm {
    /// Git default (`myers` / `default`).
    #[default]
    Myers,
    /// Minimal diff (`minimal`).
    Minimal,
    /// Patience diff (`patience`).
    Patience,
    /// Histogram diff (`histogram`).
    Histogram,
}

/// Unknown value for [`DiffAlgorithm`](DiffAlgorithm)'s [`FromStr`](std::str::FromStr) implementation.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("unknown diff algorithm `{0}`")]
pub struct UnknownDiffAlgorithm(pub String);

impl std::str::FromStr for DiffAlgorithm {
    type Err = UnknownDiffAlgorithm;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "myers" | "default" => Ok(Self::Myers),
            "minimal" => Ok(Self::Minimal),
            "patience" => Ok(Self::Patience),
            "histogram" => Ok(Self::Histogram),
            other => Err(UnknownDiffAlgorithm(other.to_owned())),
        }
    }
}

impl DiffAlgorithm {
    /// `similar` crate algorithm for non-histogram engines.
    #[must_use]
    pub fn similar_algorithm(self) -> similar::Algorithm {
        match self {
            Self::Myers | Self::Histogram => similar::Algorithm::Myers,
            Self::Minimal => similar::Algorithm::Lcs,
            Self::Patience => similar::Algorithm::Patience,
        }
    }

    /// Whether this selection uses Git's histogram engine (`imara-diff`).
    #[must_use]
    pub fn uses_git_histogram(self) -> bool {
        matches!(self, Self::Histogram)
    }
}

/// Rename detection policy for tree diffs.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameDetection {
    /// Do not detect renames.
    Off,
    /// Detect renames when similarity is at least this percent (Git `-M` default 50).
    Similarity(u8),
}

impl Default for RenameDetection {
    fn default() -> Self {
        Self::Similarity(50)
    }
}

impl RenameDetection {
    /// Git rename threshold, or `None` when renames are disabled.
    #[must_use]
    pub fn threshold_percent(self) -> Option<u32> {
        match self {
            Self::Off => None,
            Self::Similarity(p) => Some(u32::from(p)),
        }
    }
}

/// Changed-file count above which word-level inline emphasis is skipped (human TTY diffs).
pub const WORD_LEVEL_INLINE_MAX_CHANGED_FILES: usize = 64;

/// Line-diff options shared by structured output, log patches, and benchmarks.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DiffOptions {
    /// Unchanged context lines around each change (Git `-U`, default 3).
    pub context_lines: usize,
    /// `--inter-hunk-context` (default 0).
    pub inter_hunk_context: usize,
    /// Line diff algorithm.
    pub algorithm: DiffAlgorithm,
    /// Apply Git's indent heuristic when generating hunks.
    pub indent_heuristic: bool,
    /// Rename detection for tree diffs.
    pub renames: RenameDetection,
    /// `--diff-filter` status selection (applied after rename detection).
    pub filter: Option<DiffFilter>,
    /// When false, skip intra-line word emphasis (line-level hunks only).
    pub word_level_inline: bool,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            context_lines: 3,
            inter_hunk_context: 0,
            algorithm: DiffAlgorithm::Myers,
            indent_heuristic: true,
            renames: RenameDetection::default(),
            filter: None,
            word_level_inline: true,
        }
    }
}

impl DiffOptions {
    /// Builder-style update of context lines (`-U`).
    #[must_use]
    pub fn with_context_lines(mut self, context_lines: usize) -> Self {
        self.context_lines = context_lines;
        self
    }

    /// Builder-style update of inter-hunk context.
    #[must_use]
    pub fn with_inter_hunk_context(mut self, inter_hunk_context: usize) -> Self {
        self.inter_hunk_context = inter_hunk_context;
        self
    }

    /// Builder-style update of the line diff algorithm.
    #[must_use]
    pub fn with_algorithm(mut self, algorithm: DiffAlgorithm) -> Self {
        self.algorithm = algorithm;
        self
    }

    /// Builder-style update of the indent heuristic flag.
    #[must_use]
    pub fn with_indent_heuristic(mut self, indent_heuristic: bool) -> Self {
        self.indent_heuristic = indent_heuristic;
        self
    }

    /// Builder-style update of rename detection.
    #[must_use]
    pub fn with_renames(mut self, renames: RenameDetection) -> Self {
        self.renames = renames;
        self
    }

    /// Builder-style update of diff status filter.
    #[must_use]
    pub fn with_filter(mut self, filter: Option<DiffFilter>) -> Self {
        self.filter = filter;
        self
    }

    /// Git `-M` threshold in percent, or `100` when rename detection is off.
    #[must_use]
    pub fn rename_threshold_percent(self) -> u32 {
        match self.renames {
            RenameDetection::Off => 100,
            RenameDetection::Similarity(p) => u32::from(p),
        }
    }

    /// Builder-style update of word-level inline emphasis.
    #[must_use]
    pub fn with_word_level_inline(mut self, word_level_inline: bool) -> Self {
        self.word_level_inline = word_level_inline;
        self
    }

    /// Turn off word-level inline when `changed_files` exceeds
    /// [`WORD_LEVEL_INLINE_MAX_CHANGED_FILES`].
    #[must_use]
    pub fn cap_word_level_inline_for_changed_files(self, changed_files: usize) -> Self {
        let mut opts = self;
        if opts.word_level_inline && changed_files > WORD_LEVEL_INLINE_MAX_CHANGED_FILES {
            opts.word_level_inline = false;
        }
        opts
    }

    /// Read `diff.algorithm`, `diff.context`, `diff.interHunkContext`, `diff.indentHeuristic`,
    /// and `diff.renames` from repository config.
    #[must_use]
    pub fn from_config(config: &ConfigSet) -> Self {
        let mut opts = Self::default();
        if let Some(name) = config.get("diff.algorithm") {
            if let Ok(parsed) = name.parse::<DiffAlgorithm>() {
                opts.algorithm = parsed;
            }
        }
        if let Some(v) = config.get("diff.context") {
            if let Ok(n) = v.parse::<usize>() {
                opts.context_lines = n;
            }
        }
        if let Some(v) = config.get("diff.interHunkContext") {
            if let Ok(n) = v.parse::<usize>() {
                opts.inter_hunk_context = n;
            }
        }
        opts.indent_heuristic = indent_heuristic_from_config(config);
        opts.renames = renames_from_config(config);
        opts
    }

    /// Resolve indent heuristic with optional CLI overrides (log/diff plumbing).
    #[must_use]
    pub fn resolve_indent_heuristic(
        config: &ConfigSet,
        cli_indent_heuristic: bool,
        cli_no_indent_heuristic: bool,
    ) -> bool {
        resolve_indent_heuristic(config, cli_indent_heuristic, cli_no_indent_heuristic)
    }
}

/// `diff.renames`: `false` disables; `true` or copy modes use default similarity.
#[must_use]
pub fn renames_from_config(config: &ConfigSet) -> RenameDetection {
    match config.get("diff.renames") {
        Some(v) => {
            let v = v.trim();
            if v.eq_ignore_ascii_case("false")
                || v.eq_ignore_ascii_case("no")
                || v.eq_ignore_ascii_case("off")
                || v == "0"
            {
                RenameDetection::Off
            } else {
                RenameDetection::default()
            }
        }
        None => RenameDetection::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ConfigFile, ConfigScope, ConfigSet};

    fn config_from(snippet: &str) -> ConfigSet {
        let file = ConfigFile::parse(std::path::Path::new("test"), snippet, ConfigScope::Local)
            .expect("parse");
        let mut set = ConfigSet::new();
        set.merge(&file);
        set
    }

    #[test]
    fn diff_algorithm_from_str_roundtrip_and_error() {
        for (s, expected) in [
            ("myers", DiffAlgorithm::Myers),
            ("default", DiffAlgorithm::Myers),
            ("MYERS", DiffAlgorithm::Myers),
            ("minimal", DiffAlgorithm::Minimal),
            ("patience", DiffAlgorithm::Patience),
            ("histogram", DiffAlgorithm::Histogram),
        ] {
            assert_eq!(s.parse::<DiffAlgorithm>().expect("parse"), expected);
        }
        assert!(matches!(
            "bogus".parse::<DiffAlgorithm>(),
            Err(UnknownDiffAlgorithm(_))
        ));
    }

    #[test]
    fn diff_options_from_config_reads_algorithm_context_renames() {
        let cfg = config_from(
            "[diff]\n\
             \talgorithm = patience\n\
             \tcontext = 5\n\
             \tinterHunkContext = 2\n\
             \tindentHeuristic = false\n\
             \trenames = false\n",
        );
        let opts = DiffOptions::from_config(&cfg);
        assert_eq!(opts.algorithm, DiffAlgorithm::Patience);
        assert_eq!(opts.context_lines, 5);
        assert_eq!(opts.inter_hunk_context, 2);
        assert!(!opts.indent_heuristic);
        assert_eq!(opts.renames, RenameDetection::Off);

        let cfg2 = config_from("[diff]\n\trenames = copies\n");
        assert_eq!(
            DiffOptions::from_config(&cfg2).renames,
            RenameDetection::Similarity(50)
        );

        for off in ["no", "0", "off", "OFF"] {
            let cfg = config_from(&format!("[diff]\n\trenames = {off}\n"));
            assert_eq!(
                DiffOptions::from_config(&cfg).renames,
                RenameDetection::Off,
                "renames={off}"
            );
        }
    }

    #[test]
    fn cap_word_level_inline_for_changed_files() {
        let on = DiffOptions {
            word_level_inline: true,
            ..DiffOptions::default()
        };
        assert!(
            on.cap_word_level_inline_for_changed_files(WORD_LEVEL_INLINE_MAX_CHANGED_FILES)
                .word_level_inline
        );
        assert!(
            !on.cap_word_level_inline_for_changed_files(WORD_LEVEL_INLINE_MAX_CHANGED_FILES + 1)
                .word_level_inline
        );
    }
}
