//! Shared diffstat and `--stat` rendering for `show`, `log`, and `diff`.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::{FileDiff, LineKind};
use crate::markdown;
use crate::output::{outln, ChangeStatus};
use crate::ui::PathDisplayOptions;

/// Width budget for the `+`/`-` change bars in the diffstat.
const BAR_WIDTH: usize = 40;

/// A diffstat: per-file insertion/deletion counts plus totals.
#[derive(Serialize, JsonSchema, Clone)]
pub struct DiffStat {
    pub files: Vec<FileStat>,
    pub files_changed: usize,
    pub insertions: usize,
    pub deletions: usize,
}

#[derive(Serialize, JsonSchema, Clone)]
pub struct FileStat {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    pub insertions: usize,
    pub deletions: usize,
    pub binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<u32>,
}

/// Reduce a full patch diff to a per-file insertion/deletion summary.
pub fn diffstat_from_patch(diff: &[FileDiff]) -> DiffStat {
    let mut files = Vec::new();
    let mut insertions = 0;
    let mut deletions = 0;
    for file in diff {
        let (ins, del) = file
            .stat
            .map(|s| (s.insertions, s.deletions))
            .unwrap_or_else(|| counts_from_hunks(file));
        insertions += ins;
        deletions += del;
        files.push(FileStat {
            path: file.path.clone(),
            old_path: file.old_path.clone(),
            status: file.status,
            insertions: ins,
            deletions: del,
            binary: file.binary,
            similarity: file.similarity,
        });
    }
    DiffStat {
        files_changed: files.len(),
        files,
        insertions,
        deletions,
    }
}

fn counts_from_hunks(file: &FileDiff) -> (usize, usize) {
    let mut ins = 0;
    let mut del = 0;
    if let Some(hunks) = &file.hunks {
        for hunk in hunks {
            for line in &hunk.lines {
                match line.kind {
                    LineKind::Add => ins += 1,
                    LineKind::Del => del += 1,
                    LineKind::Context => {}
                }
            }
        }
    }
    (ins, del)
}

/// Build a [`DiffStat`] from stat-mode diff files (library counts).
pub fn diffstat_from_files(files: &[FileDiff]) -> DiffStat {
    diffstat_from_patch(files)
}

/// Render the diffstat, mirroring `git show --stat`.
pub fn render_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    render_stat_with_options(w, stat, color, path_opts, false)
}

/// Like [`render_stat`], optionally prefixing output with a blank line (for `grit show`).
#[allow(dead_code)]
pub fn render_stat_with_leading_blank(
    w: &mut dyn Write,
    stat: &DiffStat,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    render_stat_with_options(w, stat, color, path_opts, true)
}

fn render_stat_with_options(
    w: &mut dyn Write,
    stat: &DiffStat,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
    leading_blank: bool,
) -> Result<()> {
    if stat.files.is_empty() {
        return Ok(());
    }
    if leading_blank {
        outln!(w);
    }

    let name_width = stat
        .files
        .iter()
        .map(|f| stat_path(f, path_opts).chars().count())
        .max()
        .unwrap_or(0)
        .min(60);
    let max_changes = stat
        .files
        .iter()
        .map(|f| f.insertions + f.deletions)
        .max()
        .unwrap_or(0);

    for file in &stat.files {
        let path = stat_path(file, path_opts);
        if file.binary {
            outln!(w, " {path:<name_width$} | Bin");
            continue;
        }
        let total = file.insertions + file.deletions;
        let bar = change_bar(file.insertions, file.deletions, max_changes, color);
        outln!(w, " {path:<name_width$} | {total:>3} {bar}");
    }

    outln!(w, " {}", summary_line(stat));
    Ok(())
}

pub fn render_markdown_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    if stat.files.is_empty() {
        return Ok(());
    }
    outln!(w, "## Changes");
    outln!(w);
    outln!(w, "| File | + | − |");
    outln!(w, "| --- | ---: | ---: |");
    for file in &stat.files {
        let path = markdown::escape_table_cell(&stat_path(file, path_opts));
        let path_code = markdown::format_inline_code(&path);
        if file.binary {
            outln!(w, "| {path_code} | binary | |");
        } else {
            outln!(
                w,
                "| {path_code} | {} | {} |",
                file.insertions,
                file.deletions
            );
        }
    }
    outln!(w);
    outln!(w, "{}", summary_line(stat));
    Ok(())
}

/// Display path for `--numstat` (Git uses `old => new` for renames).
#[must_use]
pub fn numstat_path(file: &FileDiff, path_opts: Option<&PathDisplayOptions>) -> String {
    match &file.old_path {
        Some(old) => {
            let old_shown = path_opts
                .map(|o| o.format_path_for_human(old))
                .unwrap_or_else(|| old.clone());
            let new_shown = path_opts
                .map(|o| o.format_path_for_human(&file.path))
                .unwrap_or_else(|| file.path.clone());
            format!("{old_shown} => {new_shown}")
        }
        None => path_opts
            .map(|o| o.format_path_for_human(&file.path))
            .unwrap_or_else(|| file.path.clone()),
    }
}

/// `git diff --numstat` lines.
pub fn render_numstat(
    w: &mut dyn Write,
    files: &[FileDiff],
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    for file in files {
        let path = numstat_path(file, path_opts);
        if file.binary {
            outln!(w, "-\t-\t{path}");
            continue;
        }
        let (ins, del) = file
            .stat
            .map(|s| (s.insertions, s.deletions))
            .unwrap_or_else(|| counts_from_hunks(file));
        outln!(w, "{ins}\t{del}\t{path}");
    }
    Ok(())
}

pub fn render_name_only(
    w: &mut dyn Write,
    files: &[FileDiff],
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    for file in files {
        let path = path_opts
            .map(|o| o.format_path_for_human(&file.path))
            .unwrap_or_else(|| file.path.clone());
        outln!(w, "{path}");
    }
    Ok(())
}

pub fn render_name_status(
    w: &mut dyn Write,
    files: &[FileDiff],
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    for file in files {
        let path = path_opts
            .map(|o| o.format_path_for_human(&file.path))
            .unwrap_or_else(|| file.path.clone());
        let line = match file.status {
            ChangeStatus::Renamed | ChangeStatus::Copied => {
                let letter = if file.status == ChangeStatus::Renamed {
                    'R'
                } else {
                    'C'
                };
                let score = file.similarity.unwrap_or(100);
                let old = file
                    .old_path
                    .as_deref()
                    .map(|old| {
                        path_opts
                            .map(|o| o.format_path_for_human(old))
                            .unwrap_or_else(|| old.to_owned())
                    })
                    .unwrap_or_default();
                format!("{letter}{score:03}\t{old}\t{path}")
            }
            ChangeStatus::Added => format!("A\t{path}"),
            ChangeStatus::Deleted => format!("D\t{path}"),
            ChangeStatus::Modified => format!("M\t{path}"),
            ChangeStatus::TypeChanged => format!("T\t{path}"),
            ChangeStatus::Unmerged => format!("U\t{path}"),
        };
        outln!(w, "{line}");
    }
    Ok(())
}

pub fn render_shortstat(w: &mut dyn Write, stat: &DiffStat) -> Result<()> {
    if stat.files_changed > 0 {
        outln!(w, "{}", summary_line(stat));
    }
    Ok(())
}

fn stat_path(file: &FileStat, path_opts: Option<&PathDisplayOptions>) -> String {
    match &file.old_path {
        Some(old) => {
            let old_shown = path_opts
                .map(|o| o.format_path_for_human(old))
                .unwrap_or_else(|| old.clone());
            let new_shown = path_opts
                .map(|o| o.format_path_for_human(&file.path))
                .unwrap_or_else(|| file.path.clone());
            format!("{old_shown} => {new_shown}")
        }
        None => path_opts
            .map(|o| o.format_path_for_human(&file.path))
            .unwrap_or_else(|| file.path.clone()),
    }
}

fn change_bar(insertions: usize, deletions: usize, max_changes: usize, color: bool) -> String {
    let total = insertions + deletions;
    if total == 0 {
        return String::new();
    }
    let scaled = if max_changes > BAR_WIDTH {
        ((total * BAR_WIDTH).div_ceil(max_changes)).max(1)
    } else {
        total
    };
    let mut plus = ((insertions * scaled) as f64 / total as f64).round() as usize;
    let mut minus = scaled.saturating_sub(plus);
    if insertions > 0 && plus == 0 {
        plus = 1;
        minus = minus.saturating_sub(1);
    }
    if deletions > 0 && minus == 0 {
        minus = 1;
        plus = plus.saturating_sub(1);
    }
    let plus_bar = "+".repeat(plus);
    let minus_bar = "-".repeat(minus);
    if color {
        format!(
            "{}{}",
            paint(true, "32", &plus_bar),
            paint(true, "31", &minus_bar)
        )
    } else {
        format!("{plus_bar}{minus_bar}")
    }
}

pub fn summary_line(stat: &DiffStat) -> String {
    let mut parts = vec![format!(
        "{} file{} changed",
        stat.files_changed,
        plural(stat.files_changed)
    )];
    if stat.insertions > 0 {
        parts.push(format!(
            "{} insertion{}(+)",
            stat.insertions,
            plural(stat.insertions)
        ));
    }
    if stat.deletions > 0 {
        parts.push(format!(
            "{} deletion{}(-)",
            stat.deletions,
            plural(stat.deletions)
        ));
    }
    parts.join(", ")
}

fn plural(n: usize) -> &'static str {
    if n == 1 {
        ""
    } else {
        "s"
    }
}

#[must_use]
pub fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}
