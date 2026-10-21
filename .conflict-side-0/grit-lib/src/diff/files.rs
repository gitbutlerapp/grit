//! Build [`FileDiff`] lists for patch, stat, and name-only detail levels.

use crate::config::ConfigSet;
use crate::error::Result;
use crate::odb::Odb;
use crate::parallel::ThreadPolicy;

use super::detail::{DiffDetail, DiffTotals, FileDiff, LineStat};
use super::tree_content::{
    tree_diff_stat_parallel, tree_diff_structured_parallel, TreeDiffStructured,
};
use super::{DiffEntry, DiffOptions, DiffStatus};

fn mode_display(mode: &str) -> Option<String> {
    if mode == "000000" {
        None
    } else {
        Some(mode.to_owned())
    }
}

fn path_from_entry(entry: &DiffEntry) -> String {
    entry
        .new_path
        .clone()
        .or_else(|| entry.old_path.clone())
        .unwrap_or_default()
}

fn old_path_from_entry(entry: &DiffEntry, path: &str) -> Option<String> {
    entry
        .old_path
        .as_ref()
        .filter(|op| op.as_str() != path)
        .cloned()
}

fn file_diff_name_only(entry: &DiffEntry) -> FileDiff {
    let path = path_from_entry(entry);
    FileDiff {
        path: path.clone(),
        old_path: old_path_from_entry(entry, &path),
        status: entry.status,
        old_mode: mode_display(&entry.old_mode),
        new_mode: mode_display(&entry.new_mode),
        binary: false,
        similarity: entry.rename_similarity(),
        stat: None,
        structured: None,
        combined: None,
        combined_status: None,
    }
}

fn file_diff_from_structured(t: TreeDiffStructured) -> FileDiff {
    let path = path_from_entry(&t.entry);
    FileDiff {
        path: path.clone(),
        old_path: old_path_from_entry(&t.entry, &path),
        status: t.entry.status,
        old_mode: t.structured.old_mode.clone(),
        new_mode: t.structured.new_mode.clone(),
        binary: t.binary,
        similarity: t.entry.rename_similarity(),
        stat: None,
        structured: Some(t.structured),
        combined: None,
        combined_status: None,
    }
}

fn include_file_in_diff(file: &FileDiff, opts: DiffOptions) -> bool {
    if opts.whitespace.is_active() && !file.binary {
        let empty = file
            .structured
            .as_ref()
            .is_none_or(|s| s.hunks.is_empty());
        if empty {
            return false;
        }
    }
    true
}

fn file_diff_from_stat(entry: DiffEntry, binary: bool, stat: Option<LineStat>) -> FileDiff {
    let path = path_from_entry(&entry);
    FileDiff {
        path: path.clone(),
        old_path: old_path_from_entry(&entry, &path),
        status: entry.status,
        old_mode: mode_display(&entry.old_mode),
        new_mode: mode_display(&entry.new_mode),
        binary,
        similarity: entry.rename_similarity(),
        stat,
        structured: None,
        combined: None,
        combined_status: None,
    }
}

/// Output of [`build_file_diffs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffFilesOutput {
    /// Per-file results in path order.
    pub files: Vec<FileDiff>,
    /// Aggregate counts for stat detail; `None` for patch and name-only modes.
    pub totals: Option<DiffTotals>,
}

/// Fill per-file diff bodies for tree/index diff entries.
///
/// Name-only and name-status skip blob reads for line diffs; stat loads blobs for
/// counts only; patch runs the full structured line engine.
///
/// # Errors
///
/// Propagates object database and parallel diff errors.
pub fn build_file_diffs(
    odb: &Odb,
    entries: Vec<DiffEntry>,
    detail: DiffDetail,
    diff_options: DiffOptions,
    config: &ConfigSet,
    thread_policy: &ThreadPolicy,
) -> Result<DiffFilesOutput> {
    match detail {
        DiffDetail::NameOnly | DiffDetail::NameStatus => {
            let files: Vec<FileDiff> = entries.iter().map(file_diff_name_only).collect();
            Ok(DiffFilesOutput {
                files,
                totals: None,
            })
        }
        DiffDetail::Stat => {
            let rows = tree_diff_stat_parallel(odb, entries, config, thread_policy, diff_options)?;
            let files: Vec<FileDiff> = rows
                .into_iter()
                .map(|(entry, binary, stat)| file_diff_from_stat(entry, binary, stat))
                .collect();
            let totals = Some(DiffTotals::from_files(&files));
            Ok(DiffFilesOutput { files, totals })
        }
        DiffDetail::Patch => {
            let structured =
                tree_diff_structured_parallel(odb, entries, config, thread_policy, diff_options)?;
            let files: Vec<FileDiff> = structured
                .into_iter()
                .map(file_diff_from_structured)
                .filter(|f| include_file_in_diff(f, diff_options))
                .collect();
            Ok(DiffFilesOutput {
                files,
                totals: None,
            })
        }
    }
}

/// Git `--name-status` letter for a diff entry (uppercase status + rename score suffix).
#[must_use]
pub fn name_status_line(entry: &DiffEntry) -> String {
    let path = path_from_entry(entry);
    match entry.status {
        DiffStatus::Renamed => {
            let score = entry.score.unwrap_or(100);
            let old = entry.old_path.as_deref().unwrap_or("");
            format!("R{score:03}\t{old}\t{path}")
        }
        DiffStatus::Copied => {
            let score = entry.score.unwrap_or(100);
            let old = entry.old_path.as_deref().unwrap_or("");
            format!("C{score:03}\t{old}\t{path}")
        }
        DiffStatus::Added => format!("A\t{path}"),
        DiffStatus::Deleted => format!("D\t{path}"),
        DiffStatus::Modified => format!("M\t{path}"),
        DiffStatus::TypeChanged => format!("T\t{path}"),
        DiffStatus::Unmerged => format!("U\t{path}"),
    }
}
