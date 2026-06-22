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
use grit_lib::diff::{diff_tree_to_worktree, diff_trees, DiffEntry, DiffStatus};
use grit_lib::objects::ObjectId;
use grit_lib::odb::Odb;
use grit_lib::state::resolve_head;
use serde::Serialize;
use similar::{ChangeTag, TextDiff};

use crate::context;
use crate::output::HumanRender;

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
    pub binary: bool,
    pub hunks: Vec<Hunk>,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Serialize)]
pub struct Hunk {
    /// 1-based first old/new line numbers in the hunk.
    pub old_start: usize,
    pub new_start: usize,
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
    old_text: String,
    new_text: String,
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

    let mut entries = diff_tree_to_worktree(&repo.odb, head_tree.as_ref(), work_tree, &index)
        .context("could not diff the working tree")?;
    sort_entries(&mut entries);

    entries
        .into_iter()
        .map(|e| {
            let (old_text, old_bin) = blob_text(&repo.odb, &e.old_oid)?;
            let (new_text, new_bin) = match &e.new_path {
                Some(p) => file_text(&p.to_fs_path(work_tree)),
                None => (String::new(), false),
            };
            Ok(file_change(e, old_text, new_text, old_bin || new_bin))
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
            let (old_text, old_bin) = blob_text(&repo.odb, &e.old_oid)?;
            let (new_text, new_bin) = blob_text(&repo.odb, &e.new_oid)?;
            Ok(file_change(e, old_text, new_text, old_bin || new_bin))
        })
        .collect()
}

fn sort_entries(entries: &mut [DiffEntry]) {
    entries.sort_by(|a, b| a.path().cmp(b.path()));
}

fn file_change(e: DiffEntry, old_text: String, new_text: String, binary: bool) -> FileChange {
    // CLI display boundary: render byte paths lossily into the text-diff model.
    let path = e
        .new_path
        .as_deref()
        .or(e.old_path.as_deref())
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_default();
    // Record the pre-rename path only on a true rename (old differs from the
    // displayed path). For a deletion the displayed path *is* the old path, so
    // this must not treat it as a rename.
    let old_path = e
        .old_path
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .filter(|op| *op != path);
    FileChange {
        path,
        old_path,
        status: e.status,
        old_text,
        new_text,
        binary,
    }
}

/// Read a blob as text. Returns `(text, is_binary)`; a zero oid → empty text.
fn blob_text(odb: &Odb, oid: &ObjectId) -> Result<(String, bool)> {
    if *oid == ObjectId::zero() {
        return Ok((String::new(), false));
    }
    let data = odb.read(oid)?.data;
    Ok(bytes_to_text(&data))
}

/// Read a worktree file as text. A missing file reads as empty (treated as a deletion).
fn file_text(path: &Path) -> (String, bool) {
    match std::fs::read(path) {
        Ok(data) => bytes_to_text(&data),
        Err(_) => (String::new(), false),
    }
}

/// Convert bytes to `(text, is_binary)`. NUL in the first 8 KiB ⇒ binary.
fn bytes_to_text(data: &[u8]) -> (String, bool) {
    if data.iter().take(8000).any(|&b| b == 0) {
        (String::new(), true)
    } else {
        (String::from_utf8_lossy(data).into_owned(), false)
    }
}

fn status_str(status: DiffStatus) -> &'static str {
    crate::output::change_status_str(&status)
}

fn build_file_diff(fc: FileChange) -> FileDiff {
    let base = FileDiff {
        path: fc.path.clone(),
        old_path: fc.old_path.clone(),
        status: status_str(fc.status).to_owned(),
        binary: fc.binary,
        hunks: Vec::new(),
    };
    if fc.binary {
        return base;
    }

    let diff = TextDiff::from_lines(&fc.old_text, &fc.new_text);
    let mut hunks = Vec::new();
    for group in diff.grouped_ops(CONTEXT_LINES) {
        let Some(first) = group.first() else {
            continue;
        };
        let old_start = first.old_range().start;
        let new_start = first.new_range().start;

        let mut lines = Vec::new();
        for op in &group {
            for change in diff.iter_inline_changes(op) {
                let kind = match change.tag() {
                    ChangeTag::Equal => LineKind::Context,
                    ChangeTag::Delete => LineKind::Del,
                    ChangeTag::Insert => LineKind::Add,
                };
                // Drop the empty segment similar yields for the line's trailing
                // newline so the rendering and JSON stay clean.
                let segments = change
                    .iter_strings_lossy()
                    .map(|(emphasis, value)| (emphasis, strip_eol(value.as_ref()).to_owned()))
                    .filter(|(_, text)| !text.is_empty())
                    .map(|(emphasis, text)| Segment { text, emphasis })
                    .collect();
                lines.push(Line {
                    kind,
                    old: change.old_index().map(|i| i + 1),
                    new: change.new_index().map(|i| i + 1),
                    segments,
                });
            }
        }

        hunks.push(Hunk {
            old_start: old_start + 1,
            new_start: new_start + 1,
            context: funcname(&fc.old_text, old_start),
            lines,
        });
    }

    FileDiff { hunks, ..base }
}

/// Strip a trailing newline (and CR) from a segment value.
fn strip_eol(s: &str) -> &str {
    s.strip_suffix('\n')
        .map_or(s, |s| s.strip_suffix('\r').unwrap_or(s))
}

/// The nearest enclosing definition line above `start` (0-based old line index):
/// the closest preceding non-empty line that begins at column 0 with a letter
/// (functions, types, etc.). A lightweight stand-in for git's per-language
/// `xfuncname`, good enough across C/Rust/Go/JS/… for the hunk header.
fn funcname(old_text: &str, start: usize) -> Option<String> {
    let lines: Vec<&str> = old_text.split('\n').collect();
    let upto = start.min(lines.len());
    lines[..upto].iter().rev().find_map(|line| {
        let first = line.chars().next()?;
        (first.is_alphabetic() || first == '_').then(|| line.trim_end().to_owned())
    })
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

/// Color only on a TTY with `NO_COLOR` unset (https://no-color.org), and only
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

    if file.binary {
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
    if let Some(ctx) = &hunk.context {
        println!("{}", paint(color, "33", &format!("┄┄ {ctx}")));
    } else if !color {
        println!("@@ -{} +{} @@", hunk.old_start, hunk.new_start);
    }

    for line in &hunk.lines {
        println!("{}", render_line(line, width, color));
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
