//! Per-file diff detail levels (patch, stat, name-only) and aggregate totals.

use super::options::{DiffAlgorithm, DiffOptions};
use super::structured::StructuredFileDiff;
use super::{count_changes_with_algorithm, DiffEntry, DiffStatus};

/// Count insertions and deletions between two text sides using [`DiffOptions`]'s algorithm.
///
/// Does not allocate hunk structures (used for `--stat` / `--numstat`). Myers
/// comparisons trim identical boundary lines before allocating line tokens, so
/// small edits in large files need tokens only for the changed region.
#[must_use]
pub fn line_change_counts(old_content: &str, new_content: &str, options: &DiffOptions) -> LineStat {
    let (old_content, new_content) = if options.algorithm == DiffAlgorithm::Myers {
        changed_region(old_content, new_content)
    } else {
        (old_content, new_content)
    };
    if options.algorithm == DiffAlgorithm::Myers
        && !old_content.as_bytes().contains(&b'\r')
        && !new_content.as_bytes().contains(&b'\r')
    {
        return myers_lf_counts(old_content, new_content);
    }
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

fn myers_lf_counts(old: &str, new: &str) -> LineStat {
    if old.is_empty() || new.is_empty() {
        return LineStat {
            insertions: new.split_inclusive('\n').count(),
            deletions: old.split_inclusive('\n').count(),
        };
    }
    // Borrow line slices and use the same compact interner as the patch engine.
    // The text adapter otherwise scans Unicode characters and constructs a much
    // larger comparison table before Myers can run. CR-containing input keeps
    // the text adapter's special CR/CRLF tokenization above.
    let (old_ids, new_ids) = {
        let old_lines: Vec<_> = old.split_inclusive('\n').collect();
        let new_lines: Vec<_> = new.split_inclusive('\n').collect();
        let mut interner =
            crate::line_interner::LineInterner::with_capacity(old_lines.len() + new_lines.len());
        (
            interner.intern_all(&old_lines),
            interner.intern_all(&new_lines),
        )
    };
    let mut stat = LineStat::default();
    for op in similar::capture_diff_slices(similar::Algorithm::Myers, &old_ids, &new_ids) {
        match op {
            similar::DiffOp::Delete { old_len, .. } => stat.deletions += old_len,
            similar::DiffOp::Insert { new_len, .. } => stat.insertions += new_len,
            similar::DiffOp::Replace {
                old_len, new_len, ..
            } => {
                stat.deletions += old_len;
                stat.insertions += new_len;
            }
            similar::DiffOp::Equal { .. } => {}
        }
    }
    stat
}

// Myers already discards equal boundary tokens. Do that before tokenization as
// well, without allocating a token or comparison-table entry for every line.
// Keep delimiters in the comparison so an EOF newline change remains visible.
fn changed_region<'a>(old: &'a str, new: &'a str) -> (&'a str, &'a str) {
    let prefix: usize = old
        .split_inclusive('\n')
        .zip(new.split_inclusive('\n'))
        .take_while(|(a, b)| a == b)
        .map(|(line, _)| line.len())
        .sum();
    let (old, new) = (&old[prefix..], &new[prefix..]);
    let suffix: usize = old
        .split_inclusive('\n')
        .rev()
        .zip(new.split_inclusive('\n').rev())
        .take_while(|(a, b)| a == b)
        .map(|(line, _)| line.len())
        .sum();
    (&old[..old.len() - suffix], &new[..new.len() - suffix])
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

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_counts(old: &str, new: &str) -> LineStat {
        let (insertions, deletions) =
            count_changes_with_algorithm(old, new, similar::Algorithm::Myers, false);
        LineStat {
            insertions,
            deletions,
        }
    }

    #[test]
    fn myers_stat_preserves_line_boundaries_and_repeated_lines() {
        let options = DiffOptions::default();
        let texts = [
            "",
            "a",
            "a\n",
            "a\na",
            "a\na\n",
            "a\nb\na\n",
            "b\na\nb\n",
            "\n",
            "\n\n",
            "a\r",
            "a\r\n",
            "a\rb\n",
            "é\n猫",
            "é\n猫\n",
            "a\n\r\nb\n",
        ];
        for old in texts {
            for new in texts {
                assert_eq!(
                    line_change_counts(old, new, &options),
                    reference_counts(old, new),
                    "old={old:?}, new={new:?}"
                );
            }
        }

        // Cross the line-interning threshold with repeated lines and edits at
        // different offsets, including insertions beside identical boundary lines.
        let mut seed = 4687u64;
        for iteration in 0..500 {
            let mut text = || {
                (0..160)
                    .map(|_| {
                        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                        match seed >> 61 {
                            0..=3 => "repeat\n",
                            4 => "é\r\n",
                            5 => "\n",
                            6 => "x\ry\n",
                            _ => "tail\n",
                        }
                    })
                    .collect::<String>()
            };
            let (mut old, mut new) = (text(), text());
            if iteration % 2 == 0 {
                old = old.replace('\r', "");
                new = new.replace('\r', "");
            }
            let prefix = "unchanged\n".repeat(120);
            let old = format!("{prefix}{old}{prefix}");
            let new = format!("{prefix}{new}{prefix}");
            assert_eq!(
                line_change_counts(&old, &new, &options),
                reference_counts(&old, &new)
            );
        }
    }

    #[test]
    fn myers_stat_large_file_small_edit() {
        let prefix = "unchanged prefix\n".repeat(10_000);
        let suffix = "unchanged suffix\n".repeat(10_000);
        let old = format!("{prefix}old\n{suffix}");
        let new = format!("{prefix}new\nextra\n{suffix}");
        assert_eq!(changed_region(&old, &new), ("old\n", "new\nextra\n"));
        assert_eq!(
            line_change_counts(&old, &new, &DiffOptions::default()),
            LineStat {
                insertions: 2,
                deletions: 1,
            }
        );
        assert_eq!(changed_region(&old, &old), ("", ""));
    }
}
