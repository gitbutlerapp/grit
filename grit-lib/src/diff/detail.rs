//! Per-file diff detail levels (patch, stat, name-only) and aggregate totals.

use super::options::DiffOptions;
use super::structured::StructuredFileDiff;
use super::{count_line_changes_with_options, DiffEntry, DiffStatus};

/// Count insertions and deletions between two text sides using [`DiffOptions`]'s algorithm.
///
/// Does not allocate hunk structures (used for `--stat` / `--numstat`). Counts follow the
/// same compacted line alignment as structured patches for the same [`DiffOptions`].
#[must_use]
pub fn line_change_counts(old_content: &str, new_content: &str, options: &DiffOptions) -> LineStat {
    let (insertions, deletions) =
        count_line_changes_with_options(old_content, new_content, options);
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
        let lines: LineStat = files.iter().filter_map(|file| file.stat).sum();
        Self {
            files_changed: files.len(),
            insertions: lines.insertions,
            deletions: lines.deletions,
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
    use crate::diff::options::DiffAlgorithm;

    fn git_numstat(old: &str, new: &str, algorithm: DiffAlgorithm) -> LineStat {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(dir.path().join("old"), old).expect("old");
        std::fs::write(dir.path().join("new"), new).expect("new");
        let algo = match algorithm {
            DiffAlgorithm::Myers => "myers",
            DiffAlgorithm::Minimal => "minimal",
            DiffAlgorithm::Patience => "patience",
            DiffAlgorithm::Histogram => "histogram",
        };
        let out = std::process::Command::new("git")
            .args([
                "diff",
                "--no-index",
                "--numstat",
                &format!("--diff-algorithm={algo}"),
                "old",
                "new",
            ])
            .current_dir(dir.path())
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .expect("git diff");
        assert!(
            out.status.success() || out.status.code() == Some(1),
            "git diff failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        let line = String::from_utf8_lossy(&out.stdout);
        let line = line.lines().next().unwrap_or("");
        let mut parts = line.split('\t');
        let ins = parts.next().unwrap_or("0").parse().unwrap_or(0);
        let del = parts.next().unwrap_or("0").parse().unwrap_or(0);
        LineStat {
            insertions: ins,
            deletions: del,
        }
    }

    #[test]
    fn line_stat_matches_git_and_repeated_anchor_fixture() {
        let anchor = "fn anchor() {\n";
        let old = format!("{anchor}{anchor}// old body\n");
        let mut new = String::new();
        for i in 0..800 {
            new.push_str(&format!("// inserted block {i}\n"));
        }
        new.push_str(&anchor);
        new.push_str("// new tail\n");

        for algorithm in [
            DiffAlgorithm::Myers,
            DiffAlgorithm::Minimal,
            DiffAlgorithm::Patience,
            DiffAlgorithm::Histogram,
        ] {
            let options = DiffOptions::default()
                .with_algorithm(algorithm)
                .with_indent_heuristic(true);
            let grit = line_change_counts(&old, &new, &options);
            let git = git_numstat(&old, &new, algorithm);
            assert_eq!(grit, git, "{algorithm:?} numstat vs git");
        }
    }
}
