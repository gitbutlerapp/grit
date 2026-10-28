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

/// Equal line affixes trimmed off both sides, plus byte lengths on `old`.
#[must_use]
pub(crate) fn changed_region_with_offsets<'a>(
    old: &'a str,
    new: &'a str,
) -> (usize, usize, &'a str, &'a str) {
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
    (
        prefix,
        suffix,
        &old[..old.len() - suffix],
        &new[..new.len() - suffix],
    )
}

/// Myers line counts on LF-only slices (no CR); used by the stat fast path.
#[must_use]
pub(crate) fn myers_lf_line_counts(old: &str, new: &str) -> LineStat {
    if old.is_empty() || new.is_empty() {
        return LineStat {
            insertions: new.split_inclusive('\n').count(),
            deletions: old.split_inclusive('\n').count(),
        };
    }
    let old_lines: Vec<_> = old.split_inclusive('\n').collect();
    let new_lines: Vec<_> = new.split_inclusive('\n').collect();
    let mut interner =
        crate::line_interner::LineInterner::with_capacity(old_lines.len() + new_lines.len());
    let old_ids = interner.intern_all(&old_lines);
    let new_ids = interner.intern_all(&new_lines);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::options::DiffAlgorithm;
    use crate::diff::structured::{structured_file_diff, SideBytes};
    use crate::objects::FileMode;

    fn patch_oracle_stat(old: &str, new: &str, options: &DiffOptions) -> LineStat {
        let structured = structured_file_diff(
            FileMode::REGULAR,
            FileMode::REGULAR,
            &SideBytes::from_bytes(old.as_bytes().to_vec()),
            &SideBytes::from_bytes(new.as_bytes().to_vec()),
            options.clone(),
        );
        LineStat::from_hunks(&structured.hunks)
    }

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
                let grit = line_change_counts(old, new, &options);
                let patch = patch_oracle_stat(old, new, &options);
                assert_eq!(grit, patch, "patch oracle: old={old:?}, new={new:?}");
                if !old.contains('\r') && !new.contains('\r') {
                    let git = git_numstat(old, new, DiffAlgorithm::Myers);
                    assert_eq!(grit, git, "git numstat: old={old:?}, new={new:?}");
                }
            }
        }

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
            let grit = line_change_counts(&old, &new, &options);
            let patch = patch_oracle_stat(&old, &new, &options);
            assert_eq!(grit, patch, "random iteration {iteration}");
        }
    }

    #[test]
    fn myers_stat_large_file_small_edit() {
        let options = DiffOptions::default();
        let prefix = "unchanged prefix\n".repeat(10_000);
        let suffix = "unchanged suffix\n".repeat(10_000);
        let old = format!("{prefix}old\n{suffix}");
        let new = format!("{prefix}new\nextra\n{suffix}");
        let grit = line_change_counts(&old, &new, &options);
        assert_eq!(
            grit,
            LineStat {
                insertions: 2,
                deletions: 1,
            }
        );
        assert_eq!(grit, patch_oracle_stat(&old, &new, &options));
        assert_eq!(grit, git_numstat(&old, &new, DiffAlgorithm::Myers));
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
