//! [`DiffError`]: patch application, hunk selection, and blame failures.

use thiserror::Error;

use super::ApplyError;

/// Diff-area failure: applying patches, selecting hunks by id, and blame inputs.
///
/// Surfaced on the umbrella as [`Error::Diff`](crate::error::Error::Diff).
#[derive(Debug, Error)]
#[non_exhaustive]
pub enum DiffError {
    /// Patch application failure.
    #[error(transparent)]
    Apply(#[from] ApplyError),

    /// `grit diff` arguments could not be resolved into a comparison.
    #[error(transparent)]
    Arguments(#[from] crate::diff::ResolveDiffArgumentsError),

    /// A diff driver's function-name pattern does not compile.
    #[error(transparent)]
    FuncnamePattern(#[from] crate::userdiff::FuncnamePatternError),

    /// A pickaxe (`-S` / `-G`) pattern is not a valid regular expression.
    #[error(transparent)]
    Pickaxe(#[from] crate::diff::pickaxe::PickaxeError),

    /// A `--no-index` comparison of two filesystem paths failed.
    #[error(transparent)]
    NoIndex(crate::diff::NoIndexError),

    /// Partial hunk selection failed outside the stash stack.
    #[error(transparent)]
    HunkSelection(#[from] crate::porcelain::hunk_select::HunkSelectError),

    /// A diff entry whose new side is in the working tree (null new object id) has no new path,
    /// so there is no file to read.
    #[error("diff entry has no working tree path to read")]
    MissingWorktreePath,

    /// [`crate::blame::BlameFinalSource::Worktree`] was requested in a bare repository.
    #[error("cannot blame the working tree of a bare repository")]
    BlameWorktreeBareRepository,

    /// A blame `--ignore-rev` did not resolve to a commit.
    #[error("cannot find revision {rev} to ignore")]
    BlameIgnoreRevUnknown {
        /// Revision expression from `--ignore-rev`.
        rev: String,
    },

    /// A blame ignore-revs file could not be read.
    #[error("could not open object name list: {path}")]
    BlameIgnoreRevsFileOpen {
        /// Path argument (before expansion).
        path: String,
        /// Underlying open error.
        #[source]
        source: std::io::Error,
    },

    /// A line in a blame ignore-revs file is not a full-length object id.
    #[error("invalid object name: {rev}")]
    BlameIgnoreRevsFileInvalidRev {
        /// Path argument for the file.
        path: String,
        /// Offending line text after comment/trim handling.
        rev: String,
    },

    /// Blame copy detection was asked for more than the three supported levels (`-C -C -C`).
    #[error("blame copy detection supports at most three levels, got {count}")]
    BlameCopyLevelTooHigh {
        /// Number of levels requested.
        count: usize,
    },

    /// A `diff.<driver>.textconv` command could not be started.
    #[error("could not run textconv command '{command}': {source}")]
    TextconvSpawn {
        /// The configured textconv command.
        command: String,
        /// Why the command could not be started.
        #[source]
        source: std::io::Error,
    },

    /// A `diff.<driver>.textconv` command exited unsuccessfully.
    #[error("textconv command '{command}' exited with status {}", status.map_or_else(|| "unknown".to_owned(), |s| s.to_string()))]
    TextconvFailed {
        /// The configured textconv command.
        command: String,
        /// Exit status, when the command exited normally.
        status: Option<i32>,
    },
}

impl DiffError {
    /// Stable snake_case slug for this failure (the former umbrella variant names).
    #[must_use]
    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::Apply(..) => "apply",
            Self::Arguments(..) => "diff_arguments",
            Self::NoIndex(..) => "no_index",
            Self::FuncnamePattern(..) => "funcname_pattern",
            Self::Pickaxe(..) => "pickaxe",
            Self::HunkSelection(..) => "hunk_selection",
            Self::MissingWorktreePath => "diff_missing_worktree_path",
            Self::BlameWorktreeBareRepository => "blame_worktree_bare_repository",
            Self::BlameIgnoreRevUnknown { .. } => "blame_ignore_rev_unknown",
            Self::BlameIgnoreRevsFileOpen { .. } => "blame_ignore_revs_file_open",
            Self::BlameIgnoreRevsFileInvalidRev { .. } => "blame_ignore_revs_file_invalid_rev",
            Self::BlameCopyLevelTooHigh { .. } => "blame_copy_level_too_high",
            Self::TextconvSpawn { .. } => "textconv_spawn",
            Self::TextconvFailed { .. } => "textconv_failed",
        }
    }
}
