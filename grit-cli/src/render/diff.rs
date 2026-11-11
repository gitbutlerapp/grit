//! Diff outcomes, diffstat rendering, and human patch painting.

use std::io::Write;

use anyhow::Result;
use grit_lib::diff::{
    structured::{format_hunk_range, StructuredLineKind},
    DiffStat as LibDiffStat, DiffTotals, FileStat, LineStat, StatBar,
};
use schemars::JsonSchema;
use serde::Serialize;
use unicode_width::UnicodeWidthStr;

use crate::output::{outln, ChangeStatus};
use crate::render::{list, style, table, HumanRender, MarkdownRender, RenderContext};
use crate::stdio;
use crate::ui::{self, PathDisplayOptions};

const TAB_WIDTH: usize = 8;

const BG_DEL: &str = "48;5;52";
const BG_DEL_EMPH: &str = "48;5;88";
const BG_ADD: &str = "48;5;22";
const BG_ADD_EMPH: &str = "48;5;28";
const FG_DIM: &str = "38;5;244";
const FG_DEL_NUM: &str = "38;5;167";
const FG_ADD_NUM: &str = "38;5;71";
const FG_ON_DIFF: &str = "38;5;231";

/// Per-file line counts in JSON when stat modes are active.
#[derive(Clone, Copy, Serialize, JsonSchema)]
pub struct LineStatJson {
    pub insertions: usize,
    pub deletions: usize,
}

impl From<LineStat> for LineStatJson {
    fn from(stat: LineStat) -> Self {
        Self {
            insertions: stat.insertions,
            deletions: stat.deletions,
        }
    }
}

impl From<LineStatJson> for LineStat {
    fn from(stat: LineStatJson) -> Self {
        Self {
            insertions: stat.insertions,
            deletions: stat.deletions,
        }
    }
}

/// One changed file.
#[derive(Clone, Serialize, JsonSchema)]
pub struct FileDiff {
    /// Display path (the new path, or the old path for a deletion).
    pub path: String,
    /// Pre-rename path, when different from `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: ChangeStatus,
    /// Old git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    /// True when old side bytes were decoded with lossy UTF-8.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub old_encoding_lossy: bool,
    /// True when new side bytes were decoded with lossy UTF-8.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub new_encoding_lossy: bool,
    pub binary: bool,
    /// Old-side byte length when [`Self::binary`] is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub old_size: Option<u64>,
    /// New-side byte length when [`Self::binary`] is true.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub new_size: Option<u64>,
    /// True when the blob OID changed (not a mode-only touch on the same content).
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub content_changed: bool,
    /// Rename/copy similarity (0–100).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub similarity: Option<u32>,
    /// Line counts when `--stat` / `--numstat` / `--shortstat` is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub stat: Option<LineStatJson>,
    /// Per-parent combined status letters (`MM`, `DD`, …) for merge `--cc` name-status.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub combined_status: Option<String>,
    /// Patch hunks; omitted in JSON unless the output format is a full patch.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub hunks: Option<Vec<Hunk>>,
}

impl FileDiff {
    /// Line counts from the stat, or counted from the patch hunks when only those were built.
    #[must_use]
    pub fn line_stat(&self) -> LineStat {
        match self.stat {
            Some(stat) => stat.into(),
            None => self
                .hunks
                .iter()
                .flatten()
                .flat_map(|hunk| &hunk.lines)
                .map(|line| StructuredLineKind::from(line.kind))
                .collect(),
        }
    }
}

/// A contiguous run of changes plus surrounding context.
#[derive(Clone, Serialize, JsonSchema)]
pub struct Hunk {
    /// 1-based first old/new line numbers in the hunk.
    pub old_start: usize,
    pub new_start: usize,
    /// Lines on the old side in this hunk (context + removals).
    pub old_lines: usize,
    /// Lines on the new side in this hunk (context + additions).
    pub new_lines: usize,
    /// The enclosing definition line (function/struct/…), when found.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
    /// Combined (`--cc`) hunk header; when set, [`Self::old_start`] / range fields are omitted from human output.
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub combined_header: Option<String>,
    /// Stable id for `--hunks` selection (unstaged and staged diffs).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// 0-based hunk index within the file (unstaged and staged diffs).
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub index: Option<usize>,
    pub lines: Vec<Line>,
}

/// JSON schema shape for [`Line`] (`text` or `segments`, never both).
#[derive(JsonSchema)]
#[allow(dead_code)]
struct LineJsonSchema {
    kind: LineKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    new: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    segments: Option<Vec<Segment>>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    no_newline_at_eof: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    parents: Option<Vec<String>>,
}

impl JsonSchema for Line {
    fn schema_name() -> String {
        "Line".to_string()
    }

    fn json_schema(gen: &mut schemars::gen::SchemaGenerator) -> schemars::schema::Schema {
        LineJsonSchema::json_schema(gen)
    }
}

/// One rendered line of a hunk.
#[derive(Clone)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based old/new line numbers (absent on the side where the line is new/gone).
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The line split into segments; `emphasis` marks the intra-line word changes.
    pub segments: Vec<Segment>,
    /// Missing newline at end of file on this side (Git `\ No newline at end of file`).
    pub no_newline_at_eof: bool,
    /// Combined merge parent columns (`+`, `-`, or space), one entry per parent.
    pub parents: Option<Vec<String>>,
}

impl Serialize for Line {
    fn serialize<S>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        use serde::ser::SerializeStruct;
        let field_count = 3
            + usize::from(self.old.is_some())
            + usize::from(self.new.is_some())
            + usize::from(self.no_newline_at_eof)
            + usize::from(self.parents.is_some())
            + 1;
        let mut state = serializer.serialize_struct("Line", field_count)?;
        state.serialize_field("kind", &self.kind)?;
        if let Some(old) = self.old {
            state.serialize_field("old", &old)?;
        }
        if let Some(new) = self.new {
            state.serialize_field("new", &new)?;
        }
        if segments_have_emphasis(&self.segments) {
            state.serialize_field("segments", &self.segments)?;
        } else {
            state.serialize_field("text", &line_text(&self.segments))?;
        }
        if self.no_newline_at_eof {
            state.serialize_field("no_newline_at_eof", &true)?;
        }
        if let Some(parents) = &self.parents {
            state.serialize_field("parents", parents)?;
        }
        state.end()
    }
}

fn line_text(segments: &[Segment]) -> String {
    segments.iter().map(|s| s.text.as_str()).collect()
}

fn segments_have_emphasis(segments: &[Segment]) -> bool {
    segments.iter().any(|s| s.emphasis)
}

#[derive(Serialize, JsonSchema, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Add,
    Del,
}

impl From<LineKind> for StructuredLineKind {
    fn from(kind: LineKind) -> Self {
        match kind {
            LineKind::Context => Self::Context,
            LineKind::Add => Self::Add,
            LineKind::Del => Self::Del,
        }
    }
}

/// A run of text within a line, flagged if it's part of the word-level change.
#[derive(Clone, Serialize, JsonSchema)]
pub struct Segment {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub emphasis: bool,
}

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
    old_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    new_size: Option<u64>,
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
                    old_size: file.binary.then_some(file.old_size),
                    new_size: file.binary.then_some(file.new_size),
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
            .map(|file| {
                let mut stat =
                    FileStat::new(file.path.clone(), file.status.into(), file.line_stat());
                stat.old_path = file.old_path.clone();
                stat.binary = file.binary;
                stat.old_size = file.old_size.unwrap_or(0);
                stat.new_size = file.new_size.unwrap_or(0);
                stat.similarity = file.similarity;
                stat
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
        .map(|f| stat_path_display_width(f, path_opts))
        .max()
        .unwrap_or(0)
        .min(60);
    let max_changes = stat.max_changes();

    for file in stat.files() {
        let path = stat_path(file, path_opts);
        let padded = pad_stat_path(&path, name_width);
        if file.binary {
            outln!(
                w,
                " {padded} | Bin {} -> {} bytes",
                file.old_size,
                file.new_size
            );
            continue;
        }
        let total = file.lines.changes();
        let bar = paint_bar(file.lines.bar(max_changes, BAR_WIDTH), color);
        if bar.is_empty() {
            outln!(w, " {padded} | {total:>3}");
        } else {
            outln!(w, " {padded} | {total:>3} {bar}");
        }
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
        let path = table::escape_table_cell(&stat_path(file, path_opts));
        let path_code = list::format_inline_code(&path);
        if file.binary {
            outln!(
                w,
                "| {path_code} | Bin {} → {} bytes | |",
                file.old_size,
                file.new_size
            );
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

fn stat_path_display_width(file: &FileStat, path_opts: Option<&PathDisplayOptions>) -> usize {
    stat_path(file, path_opts).width()
}

fn pad_stat_path(path: &str, width: usize) -> String {
    let cols = path.width();
    let pad = width.saturating_sub(cols);
    format!("{path}{}", " ".repeat(pad))
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
            style::paint(true, "32", &plus_bar),
            style::paint(true, "31", &minus_bar)
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

/// Status word aligned with [`ChangeStatus`] JSON (`--json` / `--markdown` metadata).
#[must_use]
pub fn file_diff_status_label(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Added => "added",
        ChangeStatus::Deleted => "deleted",
        ChangeStatus::Modified => "modified",
        ChangeStatus::TypeChanged => "type_changed",
        ChangeStatus::Renamed => "renamed",
        ChangeStatus::Copied => "copied",
        ChangeStatus::Unmerged => "unmerged",
    }
}

pub fn write_file_diff_metadata_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
    match file.status {
        ChangeStatus::Deleted => outln!(w, "deleted"),
        ChangeStatus::Renamed => outln!(w, "renamed"),
        _ => {}
    }
    write_file_diff_mode_lines_human(w, file)
}

fn write_file_diff_mode_lines_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
    if file.old_mode != file.new_mode {
        if let Some(m) = &file.old_mode {
            outln!(w, "old mode {m}");
        }
        if let Some(m) = &file.new_mode {
            outln!(w, "new mode {m}");
        }
    }
    Ok(())
}

pub fn render_file(
    w: &mut dyn Write,
    file: &FileDiff,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    outln!(w);
    let header = ui::format_diff_file_header(file.old_path.as_deref(), &file.path, path_opts);
    outln!(w, "{}", style::paint(color, "1;4", &header)); // bold + underline

    write_file_diff_metadata_human(w, file)?;

    if file.binary {
        if file.content_changed {
            outln!(w, "{}", style::paint(color, FG_DIM, "Binary file differs"));
        }
        return Ok(());
    }

    // Width of each line-number column = widest number shown.
    let hunks = file.hunks.as_deref().unwrap_or(&[]);
    let width = hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .flat_map(|l| [l.old, l.new])
        .flatten()
        .max()
        .map_or(1, |n| n.to_string().len())
        .max(2);

    for hunk in hunks {
        render_hunk(w, hunk, width, color)?;
    }
    Ok(())
}

fn render_hunk(w: &mut dyn Write, hunk: &Hunk, width: usize, color: bool) -> Result<()> {
    if let Some(header) = &hunk.combined_header {
        outln!(w, "{header}");
        for line in &hunk.lines {
            let mut prefix = String::new();
            if let Some(parents) = &line.parents {
                for p in parents {
                    prefix.push_str(p);
                }
            }
            let text: String = line.segments.iter().map(|s| s.text.as_str()).collect();
            outln!(w, "{prefix}{text}");
        }
        return Ok(());
    }
    let old_rng = format_hunk_range(hunk.old_start, hunk.old_lines);
    let new_rng = format_hunk_range(hunk.new_start, hunk.new_lines);
    let range = format!("@@ -{old_rng} +{new_rng} @@");
    match (&hunk.context, color) {
        (Some(ctx), true) => {
            outln!(
                w,
                "{}",
                style::paint(color, "33", &format!("┄┄ {range} {ctx}"))
            );
        }
        (Some(ctx), false) => {
            outln!(w, "{range} {ctx}");
        }
        (None, false) => {
            outln!(w, "{range}");
        }
        (None, true) => {
            outln!(w, "{}", style::paint(color, "33", &format!("┄┄ {range}")));
        }
    }

    for line in &hunk.lines {
        write_render_line(w, line, width, color)?;
        if line.no_newline_at_eof {
            let marker = "\\ No newline at end of file";
            outln!(w, "{}", style::paint(color, FG_DIM, marker));
        }
    }
    Ok(())
}

fn write_render_line(w: &mut dyn Write, line: &Line, width: usize, color: bool) -> Result<()> {
    if color {
        outln!(w, "{}", render_line_colored(line, width));
        return Ok(());
    }
    write_plain_line(w, line, width)
}

fn write_plain_line(w: &mut dyn Write, line: &Line, width: usize) -> Result<()> {
    let sign = match line.kind {
        LineKind::Del => '-',
        LineKind::Add => '+',
        LineKind::Context => ' ',
    };
    write_plain_num_cell(w, line.old, width)?;
    stdio::io_result(write!(w, " ")).map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    write_plain_num_cell(w, line.new, width)?;
    stdio::io_result(write!(w, " │ {sign} "))
        .map_err(|e| anyhow::anyhow!("writing output: {e}"))?;
    let raw: String = line.segments.iter().map(|s| s.text.as_str()).collect();
    let text = expand_tabs(&raw, 0);
    outln!(w, "{text}");
    Ok(())
}

fn write_plain_num_cell(w: &mut dyn Write, n: Option<usize>, width: usize) -> Result<()> {
    match n {
        Some(n) => stdio::io_result(write!(w, "{n:>width$}"))
            .map_err(|e| anyhow::anyhow!("writing output: {e}"))?,
        None => stdio::io_result(write!(w, "{}", " ".repeat(width)))
            .map_err(|e| anyhow::anyhow!("writing output: {e}"))?,
    }
    Ok(())
}

fn render_line_colored(line: &Line, width: usize) -> String {
    let (base_bg, emph_bg, num_code, _sign) = match line.kind {
        LineKind::Del => (BG_DEL, BG_DEL_EMPH, FG_DEL_NUM, '-'),
        LineKind::Add => (BG_ADD, BG_ADD_EMPH, FG_ADD_NUM, '+'),
        LineKind::Context => ("", "", FG_DIM, ' '),
    };

    // Gutter: two right-aligned line-number columns (changed side colored).
    let old_col = num_cell(line.old, width, num_code, line.kind == LineKind::Del, true);
    let new_col = num_cell(line.new, width, num_code, line.kind == LineKind::Add, true);
    let bar = style::paint(true, FG_DIM, "│");
    let gutter = format!("{old_col} {new_col} {bar} ");

    // Colored: paint the background behind the text only (brighter for the
    // emphasized word-level changes). We deliberately do NOT pad to a fixed
    // width — padding past the terminal edge would wrap onto extra rows and
    // render those as blank colored bands.
    let mut body = String::new();
    let mut col = 0;
    for seg in &line.segments {
        let text = expand_tabs(&seg.text, col);
        col += text.chars().count();
        if base_bg.is_empty() {
            body.push_str(&text);
        } else {
            // Pair the dark background with an explicit light foreground so the
            // text is legible regardless of the terminal's theme.
            let bg = if seg.emphasis { emph_bg } else { base_bg };
            body.push_str(&format!("\x1b[{FG_ON_DIFF};{bg}m{text}"));
        }
    }
    if !base_bg.is_empty() {
        body.push_str(style::RESET);
    }
    format!("{gutter}{body}")
}

/// Render one line-number cell, right-aligned to `width`, colored when the line
/// is the changed side; blank when the number is absent.
fn num_cell(n: Option<usize>, width: usize, code: &str, changed: bool, color: bool) -> String {
    match n {
        Some(n) => {
            let s = format!("{n:>width$}");
            if color {
                style::paint(true, if changed { code } else { FG_DIM }, &s)
            } else {
                s
            }
        }
        None => " ".repeat(width),
    }
}

/// Expand tabs to the next [`TAB_WIDTH`] stop, given the starting column.
fn expand_tabs(s: &str, start_col: usize) -> std::borrow::Cow<'_, str> {
    if !s.contains('\t') {
        return std::borrow::Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len() + TAB_WIDTH);
    let mut col = start_col;
    for ch in s.chars() {
        if ch == '\t' {
            let n = TAB_WIDTH - (col % TAB_WIDTH);
            out.extend(std::iter::repeat_n(' ', n));
            col += n;
        } else {
            out.push(ch);
            col += 1;
        }
    }
    std::borrow::Cow::Owned(out)
}

fn format_is_byte_list_or_stat(format: crate::diff_format::DiffFormatArgs) -> bool {
    use grit_lib::diff::DiffDetail;
    format.human_numstat()
        || format.human_name_only()
        || format.human_name_status()
        || format.human_shortstat_only()
        || format.human_stat_block()
        || format.detail() == DiffDetail::Stat
        || matches!(
            format.detail(),
            DiffDetail::NameOnly | DiffDetail::NameStatus
        )
}

/// Render only the diff body (no commit header).
pub(crate) fn render_human_diff(
    outcome: &crate::commands::diff::DiffOutcome,
    w: &mut dyn Write,
    empty_message: Option<&str>,
    color: bool,
) -> Result<()> {
    if outcome.body_streamed {
        if !outcome.changed {
            if let Some(msg) = empty_message {
                outln!(w, "{msg}");
            }
        }
        return Ok(());
    }
    if outcome.files.is_empty() {
        if format_is_byte_list_or_stat(outcome.format) {
            return Ok(());
        }
        if let Some(msg) = empty_message {
            outln!(w, "{msg}");
        }
        return Ok(());
    }
    let fmt = outcome.format;
    if fmt.human_numstat() {
        return render_numstat(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_name_only() {
        return render_name_only(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_name_status() {
        return render_name_status(w, &outcome.files, outcome.path_display.as_ref());
    }
    if fmt.human_shortstat_only() {
        let stat = diffstat_from_files(&outcome.files);
        return render_shortstat(w, &stat);
    }
    if fmt.human_stat_block() {
        let stat = diffstat_from_files(&outcome.files);
        return render_stat(w, &stat, color, outcome.path_display.as_ref());
    }
    for file in &outcome.files {
        render_file(w, file, color, outcome.path_display.as_ref())?;
    }
    Ok(())
}

pub(crate) fn render_markdown_diff(
    outcome: &crate::commands::diff::DiffOutcome,
    w: &mut dyn Write,
) -> Result<()> {
    use crate::markdown;
    if outcome.body_streamed {
        if !outcome.changed {
            outln!(w, "No changes.");
        }
        return Ok(());
    }
    if outcome.files.is_empty() && format_is_byte_list_or_stat(outcome.format) {
        return Ok(());
    }
    let fmt = outcome.format;
    if fmt.human_shortstat_only() {
        let stat = diffstat_from_files(&outcome.files);
        outln!(w, "{}", summary_line(stat.lib().totals()));
        return Ok(());
    }
    if fmt.detail() == grit_lib::diff::DiffDetail::Stat {
        let stat = diffstat_from_files(&outcome.files);
        return render_markdown_stat(w, &stat, outcome.path_display.as_ref());
    }
    if fmt.human_name_status() {
        for file in &outcome.files {
            let mut buf = Vec::new();
            render_name_status(
                &mut buf,
                std::slice::from_ref(file),
                outcome.path_display.as_ref(),
            )?;
            let line = String::from_utf8(buf).unwrap_or_default();
            outln!(w, "- {}", markdown::format_inline_code(line.trim()));
        }
        return Ok(());
    }
    if fmt.human_name_only() {
        for file in &outcome.files {
            outln!(w, "- {}", markdown::format_inline_code(&file.path));
        }
        return Ok(());
    }
    markdown::print_diff_files(w, &outcome.files, outcome.path_display.as_ref())?;
    Ok(())
}

impl HumanRender for crate::commands::diff::DiffOutcome {
    fn render_human(&self, w: &mut dyn Write, ctx: &RenderContext) -> anyhow::Result<()> {
        render_human_diff(self, w, Some("No changes."), ctx.color)
    }
}

impl MarkdownRender for crate::commands::diff::DiffOutcome {
    fn render_markdown(
        &self,
        w: &mut dyn Write,
        _ctx: &crate::render::RenderContext,
    ) -> anyhow::Result<()> {
        render_markdown_diff(self, w)
    }
}

#[cfg(test)]
mod tests {
    use super::{expand_tabs, pad_stat_path};
    use std::borrow::Cow;
    use unicode_width::UnicodeWidthStr;

    #[test]
    fn expand_tabs_borrows_tab_free_text_and_aligns_tab_stops() {
        assert!(matches!(expand_tabs("plain ü text", 3), Cow::Borrowed(_)));
        assert_eq!(expand_tabs("\tx", 0), "        x");
        assert_eq!(expand_tabs("ab\tc", 0), "ab      c");
        assert_eq!(expand_tabs("ü\tc", 0), "ü       c");
        assert_eq!(expand_tabs("\t", 5), "   ");
        assert_eq!(expand_tabs("", 0), "");
    }

    #[test]
    fn stat_path_display_width_examples() {
        assert_eq!("日".width(), 2);
        assert_eq!("😀".width(), 2);
        assert_eq!("é".width(), 1);
    }

    #[test]
    fn padded_stat_paths_share_display_width() {
        let paths = ["café.txt", "emoji-😀.txt", "plain.txt", "日本語.txt"];
        let width = paths.iter().map(|p| p.width()).max().unwrap().min(60);
        let padded: Vec<_> = paths
            .iter()
            .map(|p| format!(" {}", pad_stat_path(p, width)))
            .collect();
        let pipe_cols: Vec<_> = padded.iter().map(|line| line.width() + 1).collect();
        assert!(
            pipe_cols.windows(2).all(|w| w[0] == w[1]),
            "pipe columns differ: {pipe_cols:?} for lines {padded:?}"
        );
    }
}
