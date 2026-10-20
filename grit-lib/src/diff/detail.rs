//! Per-file diff detail levels (patch, stat, name-only) and aggregate totals.

use super::options::DiffOptions;
use super::structured::StructuredFileDiff;
use super::{count_changes_with_algorithm, DiffEntry, DiffStatus};

/// Count insertions and deletions between two text sides using [`DiffOptions`]'s algorithm.
///
/// Does not allocate hunk structures (used for `--stat` / `--numstat`).
#[must_use]
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
    /// Insertion/deletion counts only (`--stat`, `--numstat`, `--shortstat`).
    Stat,
    /// `git diff --name-status` (status letter and paths).
    NameStatus,
    /// `git diff --name-only` (paths only).
    NameOnly,
}

/// Line insertion/deletion counts for one file (`--numstat`).
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

impl DiffTotals {
    /// Sum per-file stats into totals (binary files contribute to `files_changed` only).
    #[must_use]
    pub fn from_files(files: &[FileDiff]) -> Self {
        let mut insertions = 0usize;
        let mut deletions = 0usize;
        for file in files {
            if let Some(stat) = file.stat {
                insertions += stat.insertions;
                deletions += stat.deletions;
            }
        }
        Self {
            files_changed: files.len(),
            insertions,
            deletions,
        }
    }
}

/// One changed file with optional patch body or line counts.
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
    /// Structured hunks when [`DiffDetail::Patch`] was requested.
    pub structured: Option<StructuredFileDiff>,
    /// Combined (`--cc`) hunks for merge commits; mutually exclusive with [`Self::structured`].
    pub combined: Option<crate::combined_diff_patch::CombinedStructuredTextDiff>,
}

impl DiffEntry {
    /// Similarity index for renames and copies (`DiffEntry::score`).
    #[must_use]
    pub fn rename_similarity(&self) -> Option<u32> {
        match self.status {
            DiffStatus::Renamed | DiffStatus::Copied => self.score,
            _ => None,
        }
    }
}

/// Which repository comparison produced a diff (for compatibility tests).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiffTarget {
    /// Two tree objects (commit range).
    Trees,
    /// Index compared to a tree (typically `HEAD`).
    IndexToTree,
    /// Working tree compared to the index.
    IndexToWorktree,
}
