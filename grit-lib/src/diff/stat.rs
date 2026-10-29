//! Diffstat model: per-file line counts, totals, and change-bar scaling.
//!
//! [`DiffStat`] is the data behind `--stat`, `--shortstat` and `--numstat`
//! style summaries. Callers build it from listed file diffs (or from their own
//! [`FileStat`] rows) and render it however they like; [`LineStat::bar`]
//! scales each row's `+`/`-` bar against the largest change in the stat.

use std::iter::Sum;
use std::ops::{Add, AddAssign};

use super::detail::{DiffTotals, FileDiff, LineStat};
use super::structured::{StructuredFileDiff, StructuredHunk, StructuredLineKind};
use super::{DiffStatus, WhitespaceMode};
use crate::combined_diff_patch::CombinedStructuredTextDiff;

impl LineStat {
    /// Total changed lines (`insertions + deletions`).
    #[must_use]
    pub fn changes(self) -> usize {
        self.insertions + self.deletions
    }

    /// Count added and removed lines across structured hunks.
    ///
    /// # Parameters
    ///
    /// - `hunks`: hunks of one file's line diff.
    ///
    /// # Returns
    ///
    /// Insertions ([`StructuredLineKind::Add`]) and deletions
    /// ([`StructuredLineKind::Del`]); context lines are not counted.
    #[must_use]
    pub fn from_hunks(hunks: &[StructuredHunk]) -> Self {
        hunks
            .iter()
            .flat_map(|hunk| &hunk.lines)
            .map(|line| line.kind)
            .collect()
    }

    /// Scale this file's change bar against the largest change in its diffstat.
    ///
    /// Bars are drawn one column per changed line until `max_changes` exceeds
    /// `width`; then every bar is scaled by `width / max_changes`, rounding up
    /// so a changed file always gets at least one column. Within a bar the
    /// columns are split between insertions and deletions in proportion,
    /// keeping at least one column for each side that has changes.
    ///
    /// # Parameters
    ///
    /// - `max_changes`: the largest [`LineStat::changes`] among the rows drawn
    ///   together (see [`DiffStat::max_changes`]).
    /// - `width`: column budget for the widest bar.
    ///
    /// # Returns
    ///
    /// Column counts for the `+` and `-` parts; both are zero when this file
    /// has no changed lines.
    #[must_use]
    pub fn bar(self, max_changes: usize, width: usize) -> StatBar {
        let total = self.changes();
        if total == 0 {
            return StatBar::default();
        }
        let columns = if max_changes > width {
            (total * width).div_ceil(max_changes).max(1)
        } else {
            total
        };
        // Round half up: floor(insertions * columns / total + 1/2).
        let mut plus = (2 * self.insertions * columns + total) / (2 * total);
        let mut minus = columns.saturating_sub(plus);
        if self.insertions > 0 && plus == 0 {
            plus = 1;
            minus = minus.saturating_sub(1);
        }
        if self.deletions > 0 && minus == 0 {
            minus = 1;
            plus = plus.saturating_sub(1);
        }
        StatBar { plus, minus }
    }
}

impl Add for LineStat {
    type Output = Self;

    fn add(self, rhs: Self) -> Self {
        Self {
            insertions: self.insertions + rhs.insertions,
            deletions: self.deletions + rhs.deletions,
        }
    }
}

impl AddAssign for LineStat {
    fn add_assign(&mut self, rhs: Self) {
        *self = *self + rhs;
    }
}

impl Sum for LineStat {
    fn sum<I: Iterator<Item = Self>>(iter: I) -> Self {
        iter.fold(Self::default(), Add::add)
    }
}

impl FromIterator<StructuredLineKind> for LineStat {
    /// Count [`StructuredLineKind::Add`] as insertions and
    /// [`StructuredLineKind::Del`] as deletions.
    fn from_iter<I: IntoIterator<Item = StructuredLineKind>>(iter: I) -> Self {
        let mut stat = Self::default();
        for kind in iter {
            match kind {
                StructuredLineKind::Add => stat.insertions += 1,
                StructuredLineKind::Del => stat.deletions += 1,
                StructuredLineKind::Context => {}
            }
        }
        stat
    }
}

impl StructuredFileDiff {
    /// Insertions and deletions across this file's hunks.
    #[must_use]
    pub fn line_stat(&self) -> LineStat {
        LineStat::from_hunks(&self.hunks)
    }
}

impl CombinedStructuredTextDiff {
    /// Insertions and deletions of a combined (merge) diff body.
    ///
    /// A line counts as an insertion when any parent column is `+` and the
    /// line has text, and as a deletion when any parent column is `-`.
    #[must_use]
    pub fn line_stat(&self) -> LineStat {
        let mut stat = LineStat::default();
        for line in self.hunks.iter().flat_map(|hunk| &hunk.lines) {
            if line.parents.contains(&'+') && !line.text.is_empty() {
                stat.insertions += 1;
            }
            if line.parents.contains(&'-') {
                stat.deletions += 1;
            }
        }
        stat
    }
}

impl FileDiff {
    /// Line counts for this file, whichever detail level produced it.
    ///
    /// # Returns
    ///
    /// [`Self::stat`] when stat detail was computed; otherwise the counts of
    /// the structured (or combined) hunks; zero when neither is present (for
    /// example binary files and name-only listings).
    #[must_use]
    pub fn line_stat(&self) -> LineStat {
        if let Some(stat) = self.stat {
            return stat;
        }
        if let Some(structured) = &self.structured {
            return structured.line_stat();
        }
        self.combined
            .as_ref()
            .map(CombinedStructuredTextDiff::line_stat)
            .unwrap_or_default()
    }

    /// Whether `whitespace` hid every change in this file.
    ///
    /// Git leaves such files out of `--stat` and `--numstat` under `-w`, `-b`,
    /// `--ignore-space-at-eol` and `--ignore-blank-lines`. Only a text
    /// modification with unchanged modes and no remaining changed lines
    /// qualifies: additions, deletions, renames, mode changes and binary files
    /// always stay listed.
    ///
    /// # Parameters
    ///
    /// - `whitespace`: the rules the file was diffed with.
    #[must_use]
    pub fn changes_all_ignored(&self, whitespace: WhitespaceMode) -> bool {
        whitespace.is_active()
            && self.status == DiffStatus::Modified
            && !self.binary
            && self.old_mode == self.new_mode
            && self.line_stat().changes() == 0
    }
}

/// Column counts for one diffstat row's change bar (see [`LineStat::bar`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct StatBar {
    /// Columns drawn as `+` (insertions).
    pub plus: usize,
    /// Columns drawn as `-` (deletions).
    pub minus: usize,
}

/// One file's row in a [`DiffStat`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileStat {
    /// Display path (new side, or old for deletions).
    pub path: String,
    /// Pre-rename path when it differs from [`Self::path`].
    pub old_path: Option<String>,
    /// Change kind.
    pub status: DiffStatus,
    /// Either side looks like a binary blob (no line counts).
    pub binary: bool,
    /// Rename/copy similarity (0–100), when applicable.
    pub similarity: Option<u32>,
    /// Inserted and deleted lines.
    pub lines: LineStat,
}

impl From<&FileDiff> for FileStat {
    fn from(file: &FileDiff) -> Self {
        Self {
            path: file.path.clone(),
            old_path: file.old_path.clone(),
            status: file.status,
            binary: file.binary,
            similarity: file.similarity,
            lines: file.line_stat(),
        }
    }
}

/// Per-file line counts plus totals for a whole diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffStat {
    files: Vec<FileStat>,
    totals: DiffTotals,
}

impl DiffStat {
    /// Build a diffstat from file rows, summing their counts into totals.
    ///
    /// # Parameters
    ///
    /// - `files`: rows in display order.
    #[must_use]
    pub fn new(files: Vec<FileStat>) -> Self {
        let lines: LineStat = files.iter().map(|file| file.lines).sum();
        let totals = DiffTotals {
            files_changed: files.len(),
            insertions: lines.insertions,
            deletions: lines.deletions,
        };
        Self { files, totals }
    }

    /// Build a diffstat from listed file diffs at any detail level.
    ///
    /// Each row's counts come from [`FileDiff::line_stat`].
    ///
    /// # Parameters
    ///
    /// - `files`: listed file diffs, in display order.
    #[must_use]
    pub fn from_files(files: &[FileDiff]) -> Self {
        files.iter().map(FileStat::from).collect()
    }

    /// File rows in display order.
    #[must_use]
    pub fn files(&self) -> &[FileStat] {
        &self.files
    }

    /// Changed-file count and line totals across all rows.
    #[must_use]
    pub fn totals(&self) -> DiffTotals {
        self.totals
    }

    /// True when no file changed.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// The largest per-file change count: the scale for [`LineStat::bar`].
    #[must_use]
    pub fn max_changes(&self) -> usize {
        self.files
            .iter()
            .map(|file| file.lines.changes())
            .max()
            .unwrap_or(0)
    }

    /// Take the file rows, dropping the totals.
    #[must_use]
    pub fn into_files(self) -> Vec<FileStat> {
        self.files
    }
}

impl FromIterator<FileStat> for DiffStat {
    fn from_iter<I: IntoIterator<Item = FileStat>>(iter: I) -> Self {
        Self::new(iter.into_iter().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::combined_diff_patch::{
        CombinedStructuredHunk, CombinedStructuredLine, CombinedStructuredLineKind,
    };
    use crate::diff::structured::StructuredLine;

    fn lines(insertions: usize, deletions: usize) -> LineStat {
        LineStat {
            insertions,
            deletions,
        }
    }

    fn line(kind: StructuredLineKind) -> StructuredLine {
        StructuredLine {
            kind,
            old: None,
            new: None,
            segments: Vec::new(),
            no_newline_at_eof: false,
        }
    }

    fn hunk(kinds: &[StructuredLineKind]) -> StructuredHunk {
        StructuredHunk {
            old_start: 1,
            new_start: 1,
            old_lines: 0,
            new_lines: 0,
            context: None,
            lines: kinds.iter().copied().map(line).collect(),
        }
    }

    fn row(path: &str, binary: bool, stat: LineStat) -> FileStat {
        FileStat {
            path: path.to_owned(),
            old_path: None,
            status: DiffStatus::Modified,
            binary,
            similarity: None,
            lines: stat,
        }
    }

    fn listed(stat: Option<LineStat>, structured: Option<StructuredFileDiff>) -> FileDiff {
        FileDiff {
            path: "f".to_owned(),
            old_path: None,
            status: DiffStatus::Modified,
            old_mode: None,
            new_mode: None,
            binary: false,
            similarity: None,
            stat,
            structured,
            combined: None,
            combined_status: None,
        }
    }

    // The scaling rule as first written with floating-point rounding; the
    // integer form in `LineStat::bar` must agree with it everywhere.
    fn float_bar(ins: usize, del: usize, max: usize, width: usize) -> (usize, usize) {
        let total = ins + del;
        if total == 0 {
            return (0, 0);
        }
        let scaled = if max > width {
            ((total * width).div_ceil(max)).max(1)
        } else {
            total
        };
        let mut plus = ((ins * scaled) as f64 / total as f64).round() as usize;
        let mut minus = scaled.saturating_sub(plus);
        if ins > 0 && plus == 0 {
            plus = 1;
            minus = minus.saturating_sub(1);
        }
        if del > 0 && minus == 0 {
            minus = 1;
            plus = plus.saturating_sub(1);
        }
        (plus, minus)
    }

    #[test]
    fn line_stat_arithmetic() {
        assert_eq!(lines(2, 3).changes(), 5);
        assert_eq!(lines(1, 2) + lines(3, 4), lines(4, 6));
        let mut sum = lines(1, 0);
        sum += lines(0, 1);
        assert_eq!(sum, lines(1, 1));
        assert_eq!(
            [lines(1, 2), lines(3, 0)].into_iter().sum::<LineStat>(),
            lines(4, 2)
        );
        assert_eq!(
            std::iter::empty::<LineStat>().sum::<LineStat>(),
            LineStat::default()
        );
    }

    #[test]
    fn counts_structured_hunk_lines() {
        use StructuredLineKind::{Add, Context, Del};
        let hunks = [hunk(&[Context, Add, Add, Del, Context]), hunk(&[Del, Add])];
        assert_eq!(LineStat::from_hunks(&hunks), lines(3, 2));
        assert_eq!(LineStat::from_hunks(&[]), LineStat::default());
        let structured = StructuredFileDiff {
            old_mode: None,
            new_mode: None,
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            hunks: hunks.to_vec(),
        };
        assert_eq!(structured.line_stat(), lines(3, 2));
    }

    #[test]
    fn counts_combined_lines_by_parent_columns() {
        let line = |parents: &[char], text: &str| CombinedStructuredLine {
            parents: parents.to_vec(),
            text: text.to_owned(),
            kind: CombinedStructuredLineKind::from_parent_markers(parents),
        };
        let body = CombinedStructuredTextDiff {
            hunks: vec![CombinedStructuredHunk {
                header: "@@@ -1 -1 +1 @@@".to_owned(),
                lines: vec![
                    line(&['+', ' '], "added"),
                    line(&['+', '+'], ""),
                    line(&['-', ' '], "gone"),
                    line(&[' ', ' '], "same"),
                ],
            }],
        };
        assert_eq!(body.line_stat(), lines(1, 1));
    }

    #[test]
    fn listed_file_prefers_stat_then_hunks() {
        let structured = StructuredFileDiff {
            old_mode: None,
            new_mode: None,
            old_encoding_lossy: false,
            new_encoding_lossy: false,
            hunks: vec![hunk(&[StructuredLineKind::Add])],
        };
        assert_eq!(
            listed(Some(lines(7, 8)), Some(structured.clone())).line_stat(),
            lines(7, 8)
        );
        assert_eq!(listed(None, Some(structured)).line_stat(), lines(1, 0));
        assert_eq!(listed(None, None).line_stat(), LineStat::default());
    }

    #[test]
    fn changes_all_ignored_only_for_text_modifications_with_no_lines_left() {
        let ws = WhitespaceMode::ignore_space_change();
        let empty = listed(Some(LineStat::default()), None);
        assert!(empty.changes_all_ignored(ws));
        assert!(!empty.changes_all_ignored(WhitespaceMode::default()));
        assert!(!listed(Some(lines(1, 1)), None).changes_all_ignored(ws));

        let mut mode_change = empty.clone();
        mode_change.old_mode = Some("100644".to_owned());
        mode_change.new_mode = Some("100755".to_owned());
        assert!(!mode_change.changes_all_ignored(ws));

        let mut binary = empty.clone();
        binary.binary = true;
        assert!(!binary.changes_all_ignored(ws));

        for status in [DiffStatus::Added, DiffStatus::Deleted, DiffStatus::Renamed] {
            let mut other = empty.clone();
            other.status = status;
            assert!(!other.changes_all_ignored(ws), "{status:?}");
        }
    }

    #[test]
    fn diffstat_totals_and_scale() {
        let stat: DiffStat = [
            row("a", false, lines(3, 1)),
            row("bin", true, LineStat::default()),
            row("b", false, lines(0, 9)),
        ]
        .into_iter()
        .collect();
        assert_eq!(
            stat.totals(),
            DiffTotals {
                files_changed: 3,
                insertions: 3,
                deletions: 10,
            }
        );
        assert_eq!(stat.max_changes(), 9);
        assert!(!stat.is_empty());
        assert_eq!(stat.files()[1].path, "bin");
        assert_eq!(stat.into_files().len(), 3);

        let empty = DiffStat::new(Vec::new());
        assert!(empty.is_empty());
        assert_eq!(empty.max_changes(), 0);
        assert_eq!(empty.totals(), DiffTotals::default());
    }

    #[test]
    fn diffstat_from_listed_files() {
        let mut renamed = listed(Some(lines(1, 1)), None);
        renamed.status = DiffStatus::Renamed;
        renamed.old_path = Some("old".to_owned());
        renamed.similarity = Some(90);
        let stat = DiffStat::from_files(&[renamed, listed(Some(lines(2, 0)), None)]);
        assert_eq!(stat.totals().insertions, 3);
        assert_eq!(stat.totals().deletions, 1);
        let first = &stat.files()[0];
        assert_eq!(first.status, DiffStatus::Renamed);
        assert_eq!(first.old_path.as_deref(), Some("old"));
        assert_eq!(first.similarity, Some(90));
        assert!(!first.binary);
    }

    #[test]
    fn bar_is_one_column_per_line_within_width() {
        assert_eq!(lines(3, 2).bar(5, 40), StatBar { plus: 3, minus: 2 });
        assert_eq!(lines(0, 0).bar(5, 40), StatBar::default());
        assert_eq!(lines(1, 0).bar(40, 40), StatBar { plus: 1, minus: 0 });
    }

    #[test]
    fn bar_scales_and_keeps_both_sides_visible() {
        assert_eq!(lines(80, 0).bar(80, 40), StatBar { plus: 40, minus: 0 });
        assert_eq!(
            lines(40, 40).bar(80, 40),
            StatBar {
                plus: 20,
                minus: 20
            }
        );
        // A tiny change next to a huge one still gets one column.
        assert_eq!(lines(1, 0).bar(10_000, 40), StatBar { plus: 1, minus: 0 });
        // One deletion among many insertions keeps its `-` column.
        assert_eq!(lines(999, 1).bar(1000, 40), StatBar { plus: 39, minus: 1 });
        assert_eq!(lines(1, 999).bar(1000, 40), StatBar { plus: 1, minus: 39 });
    }

    #[test]
    fn bar_matches_float_rounding() {
        for width in [1, 2, 7, 40] {
            for max in 0..=120 {
                for ins in 0..=max {
                    for del in 0..=(max - ins) {
                        let bar = lines(ins, del).bar(max, width);
                        assert_eq!(
                            (bar.plus, bar.minus),
                            float_bar(ins, del, max, width),
                            "ins={ins} del={del} max={max} width={width}"
                        );
                    }
                }
            }
        }
    }
}
