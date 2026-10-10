//! `grit diff` — show changes as a delta-style, dual-line-numbered diff.
//!
//! With no argument it shows all uncommitted changes (the worktree against
//! HEAD's tree). With a commit-ish it shows the change that commit introduced
//! (its first parent's tree against its own).
//!
//! The human rendering imitates [delta](https://github.com/dandavison/delta): a
//! bold file header, a hunk header showing the enclosing definition, two
//! line-number columns (old / new), red/green line backgrounds, and brighter
//! intra-line word highlights — no `+`/`-` patch markers. Color is emitted only
//! on a TTY (honoring `NO_COLOR`); piped output is plain text with `+`/`-`.

use std::io::IsTerminal;
use std::path::Path;

use anyhow::{Context, Result};
use grit_lib::diff::{
    diff_trees, read_submodule_head_oid,
    structured::{
        format_hunk_range, structured_file_diff, SideBytes, StructuredDiffOptions,
        StructuredLineKind,
    },
    submodule_porcelain_flags, DiffEntry, DiffStatus,
};
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::state::resolve_head;
use serde::Serialize;

use crate::context;
use crate::markdown;
use crate::output::{HumanRender, MarkdownRender};

/// Lines of unchanged context to show around each change.
const CONTEXT_LINES: usize = 3;
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
#[derive(Serialize)]
pub struct DiffOutcome {
    pub files: Vec<FileDiff>,
}

/// One changed file.
#[derive(Serialize)]
pub struct FileDiff {
    /// Display path (the new path, or the old path for a deletion).
    pub path: String,
    /// Pre-rename path, when different from `path`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_path: Option<String>,
    pub status: String,
    /// Old git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_mode: Option<String>,
    /// True when old side bytes were decoded with lossy UTF-8.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub old_encoding_lossy: bool,
    /// True when new side bytes were decoded with lossy UTF-8.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub new_encoding_lossy: bool,
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Serialize)]
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
    pub context: Option<String>,
    pub lines: Vec<Line>,
}

/// One rendered line of a hunk.
#[derive(Serialize)]
pub struct Line {
    pub kind: LineKind,
    /// 1-based old/new line numbers (absent on the side where the line is new/gone).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub old: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new: Option<usize>,
    /// The line split into segments; `emphasis` marks the intra-line word changes.
    pub segments: Vec<Segment>,
    /// Missing newline at end of file on this side (Git `\ No newline at end of file`).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub no_newline_at_eof: bool,
}

#[derive(Serialize, Clone, Copy, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum LineKind {
    Context,
    Add,
    Del,
}

/// A run of text within a line, flagged if it's part of the word-level change.
#[derive(Serialize)]
pub struct Segment {
    pub text: String,
    pub emphasis: bool,
}

pub fn run(commit: Option<String>) -> Result<DiffOutcome> {
    let repo = context::discover()?;
    let changes = match commit {
        Some(spec) => {
            let oid = grit_lib::rev_parse::resolve_revision(&repo, &spec)
                .with_context(|| format!("could not resolve '{spec}'"))?;
            commit_changes(&repo, &oid)?
        }
        None => worktree_changes(&repo)?,
    };
    Ok(outcome_from_changes(changes))
}

/// The diff a single commit introduced, as a [`DiffOutcome`]. Reused by `grit show`.
pub fn diff_of_commit(repo: &grit_lib::repo::Repository, oid: &ObjectId) -> Result<DiffOutcome> {
    Ok(outcome_from_changes(commit_changes(repo, oid)?))
}

/// Build a full patch-style [`DiffOutcome`] from tree diff entries (e.g. stash W vs base).
pub fn outcome_from_tree_entries(
    repo: &grit_lib::repo::Repository,
    mut entries: Vec<DiffEntry>,
) -> Result<DiffOutcome> {
    sort_entries(&mut entries);
    let changes = entries
        .into_iter()
        .map(|e| {
            let (old_text, old_bin) = old_side_text(&repo.odb, &e)?;
            let (new_text, new_bin) = new_side_text(&repo.odb, &e, None)?;
            Ok(file_change(e, old_text, new_text, old_bin || new_bin))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(outcome_from_changes(changes))
}

fn outcome_from_changes(changes: Vec<FileChange>) -> DiffOutcome {
    DiffOutcome {
        files: changes.into_iter().map(build_file_diff).collect(),
    }
}

/// A changed file with both sides' text resolved.
struct FileChange {
    path: String,
    old_path: Option<String>,
    status: DiffStatus,
    old_mode: String,
    new_mode: String,
    old_bytes: SideBytes,
    new_bytes: SideBytes,
    binary: bool,
}

/// All uncommitted changes: HEAD's tree vs the worktree.
fn worktree_changes(repo: &grit_lib::repo::Repository) -> Result<Vec<FileChange>> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .context("grit diff needs a working tree")?;
    let head = resolve_head(&repo.git_dir).context("could not resolve HEAD")?;
    let head_tree = match head.oid() {
        Some(oid) => Some(context::commit_tree(repo, oid)?),
        None => None,
    };
    let index = repo.load_index().context("could not load the index")?;
    let config = repo.config().context("could not load config")?;

    let worktree_rules = grit_lib::worktree_rules::WorktreeRules::from_repository(repo, &index)
        .context("could not load worktree rules")?;
    let mut entries = grit_lib::diff::diff_tree_to_worktree_with_git_dir_and_rules(
        &repo.odb,
        head_tree.as_ref(),
        work_tree,
        &repo.git_dir,
        &index,
        Some(config.as_ref()),
        Some(&worktree_rules),
    )
    .context("could not diff the working tree")?;
    sort_entries(&mut entries);

    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, Some(work_tree))?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

/// The change a single commit introduced: its first parent's tree vs its own.
fn commit_changes(repo: &grit_lib::repo::Repository, oid: &ObjectId) -> Result<Vec<FileChange>> {
    let commit = context::read_commit(repo, oid)?;
    let new_tree = commit.tree;
    let old_tree = match commit.parents.first() {
        Some(parent) => Some(context::commit_tree(repo, parent)?),
        None => None,
    };

    let mut entries = diff_trees(&repo.odb, old_tree.as_ref(), Some(&new_tree), "")
        .context("could not diff the commit")?;
    sort_entries(&mut entries);

    entries
        .into_iter()
        .map(|e| {
            let (old_bytes, old_bin) = old_side_bytes(&repo.odb, &e)?;
            let (new_bytes, new_bin) = new_side_bytes(&repo.odb, &e, None)?;
            Ok(file_change(e, old_bytes, new_bytes, old_bin || new_bin))
        })
        .collect()
}

fn sort_entries(entries: &mut [DiffEntry]) {
    entries.sort_by(|a, b| a.path().cmp(b.path()));
}

fn file_change(
    e: DiffEntry,
    old_bytes: SideBytes,
    new_bytes: SideBytes,
    binary: bool,
) -> FileChange {
    let path = e
        .new_path
        .clone()
        .or_else(|| e.old_path.clone())
        .unwrap_or_default();
    // Record the pre-rename path only on a true rename (old differs from the
    // displayed path). For a deletion the displayed path *is* the old path, so
    // this must not treat it as a rename.
    let old_path = e.old_path.filter(|op| *op != path);
    FileChange {
        path,
        old_path,
        status: e.status,
        old_mode: e.old_mode,
        new_mode: e.new_mode,
        old_bytes,
        new_bytes,
        binary,
    }
}

fn old_side_bytes(odb: &Odb, e: &DiffEntry) -> Result<(SideBytes, bool)> {
    if e.old_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_line(&e.old_oid, None).into_bytes()),
            false,
        ));
    }
    if e.old_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    blob_bytes(odb, &e.old_oid)
}

fn new_side_bytes(odb: &Odb, e: &DiffEntry, work_tree: Option<&Path>) -> Result<(SideBytes, bool)> {
    if e.new_mode == "160000" {
        return Ok((
            SideBytes::from_bytes(gitlink_new_line(e, work_tree).into_bytes()),
            false,
        ));
    }
    if e.new_mode == "000000" {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    if let (Some(wt), Some(p)) = (work_tree, e.new_path.as_deref().or(e.old_path.as_deref())) {
        Ok(file_bytes(&wt.join(p)))
    } else {
        blob_bytes(odb, &e.new_oid)
    }
}

fn gitlink_line(oid: &ObjectId, dirty_suffix: Option<&str>) -> String {
    if *oid == ObjectId::zero() {
        return String::new();
    }
    let suffix = dirty_suffix.unwrap_or("");
    format!("Subproject commit {}{suffix}\n", oid.to_hex())
}

/// Resolve the worktree-side gitlink line, including `-dirty` when the submodule has modified
/// tracked content (Git patch / `--submodule` parity), including when HEAD moved ahead of the tree.
fn gitlink_new_line(e: &DiffEntry, work_tree: Option<&Path>) -> String {
    let path = e.new_path.as_deref().or(e.old_path.as_deref());
    let resolved = if e.new_oid != ObjectId::zero() {
        e.new_oid
    } else if let (Some(wt), Some(p)) = (work_tree, path) {
        read_submodule_head_oid(&wt.join(p)).unwrap_or(ObjectId::zero())
    } else {
        ObjectId::zero()
    };
    if resolved == ObjectId::zero() {
        return String::new();
    }
    let dirty = e.old_mode == "160000"
        && e.new_mode == "160000"
        && work_tree.is_some_and(|wt| {
            path.is_some_and(|p| submodule_porcelain_flags(wt, p, e.old_oid).modified)
        });
    gitlink_line(&resolved, if dirty { Some("-dirty") } else { None })
}

/// Read a blob. Returns `(bytes, is_binary)`; a zero oid → empty.
fn blob_bytes(odb: &Odb, oid: &ObjectId) -> Result<(SideBytes, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let data = odb.read(oid)?.data;
    Ok(bytes_for_diff(&data))
}

/// Read a worktree file. A missing file reads as empty (treated as a deletion).
fn file_bytes(path: &Path) -> (SideBytes, bool) {
    match std::fs::read(path) {
        Ok(data) => bytes_for_diff(&data),
        Err(_) => (SideBytes::from_bytes(Vec::new()), false),
    }
}

/// Convert bytes to `(SideBytes, is_binary)`. NUL in the first 8 KiB ⇒ binary.
fn bytes_for_diff(data: &[u8]) -> (SideBytes, bool) {
    if data.iter().take(8000).any(|&b| b == 0) {
        (SideBytes::from_bytes(Vec::new()), true)
    } else {
        (SideBytes::from_bytes(data.to_vec()), false)
    }
}

fn status_str(status: DiffStatus) -> &'static str {
    crate::output::change_status_str(&status)
}

fn build_file_diff(fc: FileChange) -> FileDiff {
    let structured = structured_file_diff(
        &fc.old_mode,
        &fc.new_mode,
        &fc.old_bytes,
        &fc.new_bytes,
        StructuredDiffOptions {
            context_lines: CONTEXT_LINES,
        },
    );
    let hunks = structured
        .hunks
        .into_iter()
        .map(|h| Hunk {
            old_start: h.old_start,
            new_start: h.new_start,
            old_lines: h.old_lines,
            new_lines: h.new_lines,
            context: h.context,
            lines: h
                .lines
                .into_iter()
                .map(|l| Line {
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
                })
                .collect(),
        })
        .collect();

    FileDiff {
        path: fc.path,
        old_path: fc.old_path,
        status: status_str(fc.status).to_owned(),
        old_mode: structured.old_mode,
        new_mode: structured.new_mode,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        binary: fc.binary,
        hunks,
    }
}

// --- Human (delta-style) rendering -----------------------------------------

impl HumanRender for DiffOutcome {
    fn render_human(&self) {
        if self.files.is_empty() {
            println!("No changes.");
            return;
        }
        let color = use_color();
        for file in &self.files {
            render_file(file, color);
        }
    }
}

impl MarkdownRender for DiffOutcome {
    fn render_markdown(&self) {
        markdown::print_diff_files(&self.files);
    }
}

/// Color only on a TTY with `NO_COLOR` unset (<https://no-color.org>), and only
/// when the console can render ANSI (always on Unix; VT-capable on Windows).
fn use_color() -> bool {
    std::io::stdout().is_terminal()
        && grit_lib::terminal::ansi_supported()
        && std::env::var_os("NO_COLOR").is_none()
}

fn paint(color: bool, code: &str, text: &str) -> String {
    if color {
        format!("\x1b[{code}m{text}{RESET}")
    } else {
        text.to_owned()
    }
}

fn render_file(file: &FileDiff, color: bool) {
    println!();
    let header = match &file.old_path {
        Some(old) => format!("{old} → {}", file.path),
        None => file.path.clone(),
    };
    println!("{}", paint(color, "1;4", &header)); // bold + underline

    if file.old_mode != file.new_mode {
        if let Some(m) = &file.old_mode {
            println!("old mode {m}");
        }
        if let Some(m) = &file.new_mode {
            println!("new mode {m}");
        }
    }

    if file.binary {
        if file.hunks.is_empty() {
            return;
        }
        println!("{}", paint(color, FG_DIM, "Binary file differs"));
        return;
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
        render_hunk(hunk, width, color);
    }
}

fn render_hunk(hunk: &Hunk, width: usize, color: bool) {
    let old_rng = format_hunk_range(hunk.old_start, hunk.old_lines);
    let new_rng = format_hunk_range(hunk.new_start, hunk.new_lines);
    let range = format!("@@ -{old_rng} +{new_rng} @@");
    match (&hunk.context, color) {
        (Some(ctx), true) => {
            println!("{}", paint(color, "33", &format!("┄┄ {range} {ctx}")));
        }
        (Some(ctx), false) => {
            println!("{range} {ctx}");
        }
        (None, false) => {
            println!("{range}");
        }
        (None, true) => {
            println!("{}", paint(color, "33", &format!("┄┄ {range}")));
        }
    }

    for line in &hunk.lines {
        println!("{}", render_line(line, width, color));
        if line.no_newline_at_eof {
            let marker = "\\ No newline at end of file";
            println!("{}", paint(color, FG_DIM, marker));
        }
    }
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
