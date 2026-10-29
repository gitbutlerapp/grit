//! Shared diffstat and `--stat` rendering for `show`, `log`, and `diff`.

use std::io::{IsTerminal, Write};

use anyhow::Result;
use grit_lib::diff::{DiffStat as LibDiffStat, DiffTotals, FileStat, StatBar};
use schemars::JsonSchema;
use serde::Serialize;

use crate::commands::diff::FileDiff;
use crate::markdown;
use crate::output::{outln, ChangeStatus};
use crate::ui::PathDisplayOptions;

/// Width budget for the `+`/`-` change bars in the diffstat.
const BAR_WIDTH: usize = 40;

/// A library [`LibDiffStat`] carried through command outcomes; serializes as
/// [`DiffStatJson`].
#[derive(Clone)]
pub struct DiffStat(LibDiffStat);

/// A diffstat: per-file insertion/deletion counts plus totals.
#[derive(Serialize, JsonSchema)]
#[serde(rename = "DiffStat")]
struct DiffStatJson {
    files: Vec<FileStatJson>,
    files_changed: usize,
    insertions: usize,
    deletions: usize,
}

#[derive(Serialize, JsonSchema)]
#[serde(rename = "FileStat")]
struct FileStatJson {
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    old_path: Option<String>,
    status: ChangeStatus,
    insertions: usize,
    deletions: usize,
    binary: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    similarity: Option<u32>,
}

impl From<&DiffStat> for DiffStatJson {
    fn from(DiffStat(stat): &DiffStat) -> Self {
        let totals = stat.totals();
        Self {
            files: stat
                .files()
                .iter()
                .map(|file| FileStatJson {
                    path: file.path.clone(),
                    old_path: file.old_path.clone(),
                    status: file.status.into(),
                    insertions: file.lines.insertions,
                    deletions: file.lines.deletions,
                    binary: file.binary,
                    similarity: file.similarity,
                })
                .collect(),
            files_changed: totals.files_changed,
            insertions: totals.insertions,
            deletions: totals.deletions,
        }
    }
}

impl Serialize for DiffStat {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        DiffStatJson::from(self).serialize(serializer)
    }
}

impl JsonSchema for DiffStat {
    fn schema_name() -> String {
        DiffStatJson::schema_name()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        DiffStatJson::json_schema(gen)
    }
}

impl From<LibDiffStat> for DiffStat {
    fn from(stat: LibDiffStat) -> Self {
        Self(stat)
    }
}

impl DiffStat {
    /// The library model behind this diffstat.
    #[must_use]
    pub fn lib(&self) -> &LibDiffStat {
        &self.0
    }
}

/// Build a [`DiffStat`] from CLI diff files at any detail level.
pub fn diffstat_from_files(files: &[FileDiff]) -> DiffStat {
    DiffStat(
        files
            .iter()
            .map(|file| FileStat {
                path: file.path.clone(),
                old_path: file.old_path.clone(),
                status: file.status.into(),
                binary: file.binary,
                similarity: file.similarity,
                lines: file.line_stat(),
            })
            .collect(),
    )
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
    let stat = stat.lib();
    if stat.is_empty() {
        return Ok(());
    }
    if leading_blank {
        outln!(w);
    }

    let name_width = stat
        .files()
        .iter()
        .map(|f| stat_path(f, path_opts).chars().count())
        .max()
        .unwrap_or(0)
        .min(60);
    let max_changes = stat.max_changes();

    for file in stat.files() {
        let path = stat_path(file, path_opts);
        if file.binary {
            outln!(w, " {path:<name_width$} | Bin");
            continue;
        }
        let total = file.lines.changes();
        let bar = paint_bar(file.lines.bar(max_changes, BAR_WIDTH), color);
        outln!(w, " {path:<name_width$} | {total:>3} {bar}");
    }

    outln!(w, " {}", summary_line(stat.totals()));
    Ok(())
}

pub fn render_markdown_stat(
    w: &mut dyn Write,
    stat: &DiffStat,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    let stat = stat.lib();
    if stat.is_empty() {
        return Ok(());
    }
    outln!(w, "## Changes");
    outln!(w);
    outln!(w, "| File | + | − |");
    outln!(w, "| --- | ---: | ---: |");
    for file in stat.files() {
        let path = markdown::escape_table_cell(&stat_path(file, path_opts));
        let path_code = markdown::format_inline_code(&path);
        if file.binary {
            outln!(w, "| {path_code} | binary | |");
        } else {
            outln!(
                w,
                "| {path_code} | {} | {} |",
                file.lines.insertions,
                file.lines.deletions
            );
        }
    }
    outln!(w);
    outln!(w, "{}", summary_line(stat.totals()));
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
        let lines = file.line_stat();
        outln!(w, "{}\t{}\t{path}", lines.insertions, lines.deletions);
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
        if let Some(ref letters) = file.combined_status {
            outln!(w, "{letters}\t{path}");
            continue;
        }
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
    let totals = stat.lib().totals();
    if totals.files_changed > 0 {
        outln!(w, "{}", summary_line(totals));
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

fn paint_bar(bar: StatBar, color: bool) -> String {
    if bar == StatBar::default() {
        return String::new();
    }
    let plus_bar = "+".repeat(bar.plus);
    let minus_bar = "-".repeat(bar.minus);
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

pub fn summary_line(totals: DiffTotals) -> String {
    let mut parts = vec![format!(
        "{} file{} changed",
        totals.files_changed,
        plural(totals.files_changed)
    )];
    if totals.insertions > 0 {
        parts.push(format!(
            "{} insertion{}(+)",
            totals.insertions,
            plural(totals.insertions)
        ));
    }
    if totals.deletions > 0 {
        parts.push(format!(
            "{} deletion{}(-)",
            totals.deletions,
            plural(totals.deletions)
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
