//! Commit show / merge diff file shapes (patch, stat, combined `--cc`).
//!
//! Separate from [`super::targets::FileDiff`] so merge combined hunks can carry
//! [`crate::combined_diff_patch::CombinedStructuredTextDiff`] without changing
//! the primary diff API.

use super::options::DiffOptions;
use super::structured::StructuredFileDiff;
use super::{count_changes_with_algorithm, DiffStatus};

/// Count insertions and deletions between two text sides using [`DiffOptions`]'s algorithm.
#[must_use]
#[allow(dead_code)]
pub fn line_change_counts(old_content: &str, new_content: &str, options: &DiffOptions) -> LineStat {
    let (insertions, deletions) = count_changes_with_algorithm(
        old_content,
        new_content,
        options.algorithm.similar_algorithm(),
        options.algorithm.uses_git_histogram(),
    );
    LineStat {
        insertions,
        deletions,
    }
}

/// How much content to compute for each changed file.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffDetail {
    /// Full structured line hunks.
    #[default]
    Patch,
    /// Insertion/deletion counts only.
    Stat,
    /// Status letter and paths.
    NameStatus,
    /// Paths only.
    NameOnly,
}

/// Line insertion/deletion counts for one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LineStat {
    /// Lines added on the new side.
    pub insertions: usize,
    /// Lines removed from the old side.
    pub deletions: usize,
}

/// Summary across all files in a stat-style diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct DiffTotals {
    /// Number of changed paths.
    pub files_changed: usize,
    /// Total insertions across text files.
    pub insertions: usize,
    /// Total deletions across text files.
    pub deletions: usize,
}

/// One changed file with optional patch body or line counts for commit show.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Display path (new side, or old for deletions).
    pub path: String,
    /// Pre-rename path when it differs from [`Self::path`].
    pub old_path: Option<String>,
    /// Change kind.
    pub status: DiffStatus,
    /// Old git mode (octal), when present.
    pub old_mode: Option<String>,
    /// New git mode (octal), when present.
    pub new_mode: Option<String>,
    /// Either side looks like a binary blob.
    pub binary: bool,
    /// Rename/copy similarity (0–100), when applicable.
    pub similarity: Option<u32>,
    /// Line counts when [`DiffDetail::Stat`] was requested.
    pub stat: Option<LineStat>,
    /// Structured hunks when [`DiffDetail::Patch`] was requested (first-parent).
    pub structured: Option<StructuredFileDiff>,
    /// Combined (`--cc`) hunks for merge commits; mutually exclusive with [`Self::structured`].
    pub combined: Option<crate::combined_diff_patch::CombinedStructuredTextDiff>,
}

/// Output of [`super::merge_commit::commit_diff_files`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFilesOutput {
    /// Per-file results in path order.
    pub files: Vec<FileDiff>,
    /// Aggregate counts for stat detail; `None` for patch and name-only modes.
    pub totals: Option<DiffTotals>,
}
