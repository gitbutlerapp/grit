//! `grit diff` — show changes as a delta-style, dual-line-numbered diff.
//!
//! With no argument it shows all uncommitted changes (the worktree and index
//! against HEAD's tree). Pass `--unstaged` for index against worktree only, or
//! `--staged` for the index against HEAD. With one commit-ish
//! it shows the change that commit introduced (its first parent's tree against
//! its own). Two commits, or a two-dot range such as `A..B`, diff those
//! commits' trees. Use `--against <ref>` to compare the worktree to another
//! ref's tree instead of HEAD.
//!
//! The human rendering imitates [delta](https://github.com/dandavison/delta): a
//! bold file header, a hunk header showing the enclosing definition, two
//! line-number columns (old / new), red/green line backgrounds, and brighter
//! intra-line word highlights — no `+`/`-` patch markers. Color is emitted only
//! on a TTY (honoring `NO_COLOR`); piped output is plain text with `+`/`-`.

use crate::context;
use crate::diff_args::DiffFormatArgs;
use crate::markdown;
use crate::output::{outln, ChangeStatus, HumanRender, MarkdownRender, RenderContext};
use crate::ui::{self, PathDisplayOptions};
use anyhow::{Context, Result};
use grit_lib::diff::structured::{format_hunk_range, StructuredHunk, StructuredLineKind};
use grit_lib::diff::{resolve_diff_arguments, DiffEntry, DiffOptions, DiffTarget};
use grit_lib::objects::ObjectId;
use grit_lib::worktree::pathspec::pathdiff;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::IsTerminal;
use std::io::Write;

/// Tab stop width used when expanding tabs for display.
const TAB_WIDTH: usize = 8;

// --- ANSI styling (256-color, delta-ish) -----------------------------------
const RESET: &str = "\x1b[0m";
const BG_DEL: &str = "48;5;52"; // dark red
const BG_DEL_EMPH: &str = "48;5;88"; // brighter red
const BG_ADD: &str = "48;5;22"; // dark green
const BG_ADD_EMPH: &str = "48;5;28"; // brighter green
const FG_DIM: &str = "38;5;244"; // gutter gray
const FG_DEL_NUM: &str = "38;5;167"; // removed line number
const FG_ADD_NUM: &str = "38;5;71"; // added line number
                                    // Explicit near-white foreground for the changed (colored-background) lines.
                                    // Our backgrounds are always dark, so a light fg stays readable on both light-
                                    // and dark-themed terminals (the default fg would be dark-on-dark in light mode).
const FG_ON_DIFF: &str = "38;5;231";

/// Result of `grit diff`.
#[derive(Serialize, JsonSchema)]
pub struct DiffOutcome {
    pub files: Vec<FileDiff>,
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

/// One changed file.
#[derive(Serialize, JsonSchema)]
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
    /// True when the blob OID changed (not a mode-only touch on the same content).
    #[serde(skip_serializing)]
    #[schemars(skip)]
    pub content_changed: bool,
    pub hunks: Vec<Hunk>,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Serialize, JsonSchema)]
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
pub struct Line {
    pub kind: LineKind,
    /// 1-based old/new line numbers (absent on the side where the line is new/gone).
    pub old: Option<usize>,
    pub new: Option<usize>,
    /// The line split into segments; `emphasis` marks the intra-line word changes.
    pub segments: Vec<Segment>,
    /// Missing newline at end of file on this side (Git `\ No newline at end of file`).
    pub no_newline_at_eof: bool,
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

/// A run of text within a line, flagged if it's part of the word-level change.
#[derive(Serialize, JsonSchema)]
pub struct Segment {
    pub text: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[schemars(default)]
    pub emphasis: bool,
}

/// Inputs for `grit diff` (flags and revision arguments).
pub struct DiffRunOptions {
    pub staged: bool,
    pub unstaged: bool,
    pub against: Option<String>,
    pub commits: Vec<String>,
    /// Pathspecs from trailing `PATH` arguments (after `--`).
    pub paths: Vec<String>,
    /// Unified context and related line-diff flags.
    pub format: DiffFormatArgs,
    /// Intra-line word highlights in structured hunks (human TTY output only).
    pub word_level_inline: bool,
}

/// Whether human diff output should compute word-level inline emphasis.
#[must_use]
pub fn diff_word_level_inline_for_output() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn attach_path_display(repo: &grit_lib::repo::Repository, mut outcome: DiffOutcome) -> DiffOutcome {
    outcome.path_display = PathDisplayOptions::from_repo(repo).ok();
    outcome
}

fn pathspec_prefix_for_repo(repo: &grit_lib::repo::Repository) -> Result<Option<String>> {
    let Some(work_tree) = repo.work_tree() else {
        return Ok(None);
    };
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    Ok(pathdiff(&cwd, work_tree))
}

pub fn run(opts: DiffRunOptions) -> Result<DiffOutcome> {
    let repo = context::discover()?;
    let config = repo.config().context("could not load config")?;
    let prefix = pathspec_prefix_for_repo(&repo)?;
    let mut diff_options = opts
        .format
        .apply_to(DiffOptions::from_config(config.as_ref()), config.as_ref())?;
    diff_options.word_level_inline = opts.word_level_inline;
    let args = resolve_diff_arguments(
        &repo,
        prefix.as_deref(),
        opts.staged,
        opts.unstaged,
        opts.against.as_deref(),
        &opts.commits,
        &opts.paths,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    let files = repo
        .diff()
        .files(&args.target, &args.pathspecs, &diff_options)
        .context("could not build diff")?;
    Ok(attach_path_display(
        &repo,
        DiffOutcome {
            files: files.into_iter().map(FileDiff::from).collect(),
            path_display: None,
        },
    ))
}

/// The diff a single commit introduced, as a [`DiffOutcome`]. Reused by `grit show`.
pub fn diff_of_commit(repo: &grit_lib::repo::Repository, oid: &ObjectId) -> Result<DiffOutcome> {
    let config = repo.config().context("could not load config")?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.word_level_inline = false;
    let files = repo
        .diff()
        .files(&DiffTarget::Commit(*oid), &[], &diff_options)
        .context("could not build diff")?;
    Ok(attach_path_display(
        repo,
        DiffOutcome {
            files: files.into_iter().map(FileDiff::from).collect(),
            path_display: None,
        },
    ))
}

/// Build a full patch-style [`DiffOutcome`] from tree diff entries (e.g. stash W vs base).
pub fn outcome_from_tree_entries(
    repo: &grit_lib::repo::Repository,
    entries: Vec<DiffEntry>,
) -> Result<DiffOutcome> {
    let config = repo.config().context("could not load config")?;
    let mut diff_options = DiffOptions::from_config(config.as_ref());
    diff_options.word_level_inline = false;
    let files = repo
        .diff()
        .files_from_entries(entries, &diff_options)
        .context("could not build diff")?;
    Ok(attach_path_display(
        repo,
        DiffOutcome {
            files: files.into_iter().map(FileDiff::from).collect(),
            path_display: None,
        },
    ))
}

impl From<grit_lib::diff::FileDiff> for FileDiff {
    fn from(f: grit_lib::diff::FileDiff) -> Self {
        Self {
            path: f.path,
            old_path: f.old_path,
            status: f.status.into(),
            old_mode: f.old_mode,
            new_mode: f.new_mode,
            old_encoding_lossy: f.old_encoding_lossy,
            new_encoding_lossy: f.new_encoding_lossy,
            binary: f.binary,
            content_changed: f.content_changed,
            hunks: f.hunks.into_iter().map(Hunk::from).collect(),
        }
    }
}

impl From<StructuredHunk> for Hunk {
    fn from(h: StructuredHunk) -> Self {
        Self {
            old_start: h.old_start,
            new_start: h.new_start,
            old_lines: h.old_lines,
            new_lines: h.new_lines,
            context: h.context,
            lines: h.lines.into_iter().map(Line::from).collect(),
        }
    }
}

impl From<grit_lib::diff::structured::StructuredLine> for Line {
    fn from(l: grit_lib::diff::structured::StructuredLine) -> Self {
        Self {
            kind: match l.kind {
                StructuredLineKind::Context => LineKind::Context,
                StructuredLineKind::Add => LineKind::Add,
                StructuredLineKind::Del => LineKind::Del,
            },
            old: l.old,
            new: l.new,
            segments: l
                .segments
                .into_iter()
                .map(|s| Segment {
                    text: s.text,
                    emphasis: s.emphasis,
                })
                .collect(),
            no_newline_at_eof: l.no_newline_at_eof,
        }
    }
}

// --- Human (delta-style) rendering -----------------------------------------

impl HumanRender for DiffOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.files.is_empty() {
            outln!(w, "No changes.");
            return Ok(());
        }
        let color = use_color();
        for file in &self.files {
            render_file(w, file, color, self.path_display.as_ref())?;
        }
        Ok(())
    }
}

impl MarkdownRender for DiffOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_diff_files(w, &self.files, self.path_display.as_ref())?;
        Ok(())
    }
}

/// Color only on a TTY with `NO_COLOR` unset (<https://no-color.org>), and only
/// when the console can render ANSI (always on Unix; VT-capable on Windows).
fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::porcelain::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

/// Status word aligned with [`ChangeStatus`] JSON (`--json` / `--markdown` metadata).
#[must_use]
pub(crate) fn file_diff_status_label(status: ChangeStatus) -> &'static str {
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

pub(crate) fn write_file_diff_metadata_human(w: &mut dyn Write, file: &FileDiff) -> Result<()> {
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

fn render_file(
    w: &mut dyn Write,
    file: &FileDiff,
    color: bool,
    path_opts: Option<&PathDisplayOptions>,
) -> Result<()> {
    outln!(w);
    let header = ui::format_diff_file_header(file.old_path.as_deref(), &file.path, path_opts);
    outln!(w, "{}", paint(color, "1;4", &header)); // bold + underline

    write_file_diff_metadata_human(w, file)?;

    if file.binary {
        if file.content_changed {
            outln!(w, "{}", paint(color, FG_DIM, "Binary file differs"));
        }
        return Ok(());
    }

    // Width of each line-number column = widest number shown.
    let width = file
        .hunks
        .iter()
        .flat_map(|h| h.lines.iter())
        .flat_map(|l| [l.old, l.new])
        .flatten()
        .max()
        .map_or(1, |n| n.to_string().len())
        .max(2);

    for hunk in &file.hunks {
        render_hunk(w, hunk, width, color)?;
    }
    Ok(())
}

fn render_hunk(w: &mut dyn Write, hunk: &Hunk, width: usize, color: bool) -> Result<()> {
    let old_rng = format_hunk_range(hunk.old_start, hunk.old_lines);
    let new_rng = format_hunk_range(hunk.new_start, hunk.new_lines);
    let range = format!("@@ -{old_rng} +{new_rng} @@");
    match (&hunk.context, color) {
        (Some(ctx), true) => {
            outln!(w, "{}", paint(color, "33", &format!("┄┄ {range} {ctx}")));
        }
        (Some(ctx), false) => {
            outln!(w, "{range} {ctx}");
        }
        (None, false) => {
            outln!(w, "{range}");
        }
        (None, true) => {
            outln!(w, "{}", paint(color, "33", &format!("┄┄ {range}")));
        }
    }

    for line in &hunk.lines {
        outln!(w, "{}", render_line(line, width, color));
        if line.no_newline_at_eof {
            let marker = "\\ No newline at end of file";
            outln!(w, "{}", paint(color, FG_DIM, marker));
        }
    }
    Ok(())
}

fn render_line(line: &Line, width: usize, color: bool) -> String {
    let (base_bg, emph_bg, num_code, sign) = match line.kind {
        LineKind::Del => (BG_DEL, BG_DEL_EMPH, FG_DEL_NUM, '-'),
        LineKind::Add => (BG_ADD, BG_ADD_EMPH, FG_ADD_NUM, '+'),
        LineKind::Context => ("", "", FG_DIM, ' '),
    };

    // Gutter: two right-aligned line-number columns (changed side colored).
    let old_col = num_cell(line.old, width, num_code, line.kind == LineKind::Del, color);
    let new_col = num_cell(line.new, width, num_code, line.kind == LineKind::Add, color);
    let bar = paint(color, FG_DIM, "│");
    let gutter = format!("{old_col} {new_col} {bar} ");

    if !color {
        // Plain text: lead with a +/-/space marker so piped output is readable.
        let raw: String = line.segments.iter().map(|s| s.text.as_str()).collect();
        return format!("{gutter}{sign} {}", expand_tabs(&raw, 0));
    }

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
        body.push_str(RESET);
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
                paint(true, if changed { code } else { FG_DIM }, &s)
            } else {
                s
            }
        }
        None => " ".repeat(width),
    }
}

/// Expand tabs to the next [`TAB_WIDTH`] stop, given the starting column.
fn expand_tabs(s: &str, start_col: usize) -> String {
    let mut out = String::new();
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
    out
}
