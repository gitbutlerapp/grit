//! Per-file diff detail levels (patch, stat, name-only) and aggregate totals.

use super::has_changes::text_sides_differ;
use super::options::DiffOptions;
use super::structured::StructuredFileDiff;
use super::{count_changes_with_algorithm, DiffEntry, DiffStatus, WhitespaceMode};

/// Count insertions and deletions between two text sides using [`DiffOptions`]'s algorithm.
///
/// Does not allocate hunk structures (used for `--stat` / `--numstat`).
#[must_use]
pub fn line_change_counts(old_content: &str, new_content: &str, options: &DiffOptions) -> LineStat {
    if !text_sides_differ(old_content, new_content, options) {
        return LineStat::default();
    }
    let (old_content, new_content) =
        sides_for_line_stat(old_content, new_content, options.whitespace);
    let (insertions, deletions) = count_changes_with_algorithm(
        &old_content,
        &new_content,
        options.algorithm.similar_algorithm(),
        options.algorithm.uses_git_histogram(),
    );
    LineStat {
        insertions,
        deletions,
    }
}

/// Line-oriented normalization for `--stat` / `--numstat` when whitespace rules apply.
///
/// Keeps one `\n` per logical line so line diffs match Git (avoids merging the last old line
/// with the first new line when trailing newlines differ).
fn sides_for_line_stat(old: &str, new: &str, ws: WhitespaceMode) -> (String, String) {
    if !ws.is_active() {
        return (old.to_owned(), new.to_owned());
    }
    let normalize_side = |text: &str| -> String {
        let mut lines: Vec<String> = text
            .lines()
            .map(|line| ws.normalize_line_for_compare(line).into_owned())
            .collect();
        if ws.ignore_blank_lines {
            lines.retain(|l| !l.trim().is_empty());
        }
        if lines.is_empty() {
            String::new()
        } else {
            lines.join("\n") + "\n"
        }
    };
    (normalize_side(old), normalize_side(new))
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
    /// Per-parent status letters for combined merge name-status (e.g. `MM`, `DD`).
    pub combined_status: Option<String>,
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
