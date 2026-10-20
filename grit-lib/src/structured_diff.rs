//! Structured line diffs for CLI and embedders (`grit diff`, `grit show`).
//!
//! Preserves line-ending details (CRLF vs LF, missing newline at EOF), mode-only
//! changes, hunk line counts, and optional UTF-8 lossy decoding markers — without
//! relying on unified patch text parsing.

use std::borrow::Cow;

use similar::{ChangeTag, TextDiff};

use crate::diff::options::DiffOptions;

#[path = "diff/line_engine.rs"]
mod line_engine;

/// Bytes of one file side prepared for diffing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SideBytes {
    /// Raw file contents.
    pub data: Vec<u8>,
    /// `true` when [`SideBytes::text`] used lossy UTF-8 decoding.
    pub encoding_lossy: bool,
}

impl SideBytes {
    /// Decode as text for diffing (NUL in the first 8 KiB ⇒ treat as binary upstream).
    #[must_use]
    pub fn from_bytes(data: Vec<u8>) -> Self {
        let encoding_lossy = !data.is_empty() && std::str::from_utf8(&data).is_err();
        Self {
            data,
            encoding_lossy,
        }
    }

    /// Lossy UTF-8 view of [`Self::data`].
    #[must_use]
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.data).into_owned()
    }
}

/// One run of text within a line, with optional intra-line emphasis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredSegment {
    /// Text for this item.
    pub text: String,
    /// Emphasis for this item.
    pub emphasis: bool,
}

/// Kind of line in a structured hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredLineKind {
    /// Returned when context.
    Context,
    /// Returned when add.
    Add,
    /// Returned when del.
    Del,
}

/// One rendered line in a structured hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredLine {
    /// Object type name when known (`commit`, `tree`, etc.).
    pub kind: StructuredLineKind,
    /// Old for this item.
    pub old: Option<usize>,
    /// New for this item.
    pub new: Option<usize>,
    /// Segments for this item.
    pub segments: Vec<StructuredSegment>,
    /// Git `\ No newline at end of file` on this side.
    pub no_newline_at_eof: bool,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredHunk {
    /// Old start for this item.
    pub old_start: usize,
    /// New start for this item.
    pub new_start: usize,
    /// Old lines for this item.
    pub old_lines: usize,
    /// New lines for this item.
    pub new_lines: usize,
    /// Context for this item.
    pub context: Option<String>,
    /// Lines for this item.
    pub lines: Vec<StructuredLine>,
}

/// Line-level diff of two text sides (no file metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredTextDiff {
    /// Hunks for this item.
    pub hunks: Vec<StructuredHunk>,
}

/// File metadata plus a structured line diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredFileDiff {
    /// Old mode for this item.
    pub old_mode: Option<String>,
    /// New mode for this item.
    pub new_mode: Option<String>,
    /// Old encoding lossy for this item.
    pub old_encoding_lossy: bool,
    /// New encoding lossy for this item.
    pub new_encoding_lossy: bool,
    /// Hunks for this item.
    pub hunks: Vec<StructuredHunk>,
}

/// Pre-split line bodies for O(1) lookup while building hunks.
#[derive(Debug, Clone)]
struct TextLines {
    lines: Vec<String>,
    #[allow(dead_code)]
    ends_with_newline: bool,
}

impl TextLines {
    fn from_text(text: &str) -> Self {
        let (parts, ends_nl) = lines_with_trailing_newline(text);
        Self {
            lines: parts
                .iter()
                .map(|s| strip_diff_newline_only(s).to_owned())
                .collect(),
            ends_with_newline: ends_nl,
        }
    }
}

/// Decode side bytes as UTF-8 when valid to avoid an extra allocation on the hot path.
fn side_text_for_diff(side: &SideBytes) -> Cow<'_, str> {
    if side.data.is_empty() {
        return Cow::Borrowed("");
    }
    match std::str::from_utf8(&side.data) {
        Ok(text) => Cow::Borrowed(text),
        Err(_) => Cow::Owned(String::from_utf8_lossy(&side.data).into_owned()),
    }
}

/// Like [`structured_text_diff`] but accepts borrowed text when sides are valid UTF-8.
fn structured_text_diff_cow(
    old_text: Cow<'_, str>,
    new_text: Cow<'_, str>,
    options: DiffOptions,
) -> StructuredTextDiff {
    structured_text_diff(old_text.as_ref(), new_text.as_ref(), options)
}

/// Build a structured line diff from two decoded text sides.
#[must_use]
pub fn structured_text_diff(
    old_text: &str,
    new_text: &str,
    options: DiffOptions,
) -> StructuredTextDiff {
    let mut hunks = line_engine::line_hunks(old_text, new_text, &options, options.function_context);
    if options.word_level_inline {
        apply_word_level_inline(&mut hunks, old_text, new_text);
    }
    StructuredTextDiff { hunks }
}

fn apply_word_level_inline(hunks: &mut [StructuredHunk], old_text: &str, new_text: &str) {
    let old = TextLines::from_text(old_text);
    let new = TextLines::from_text(new_text);
    let old_snapshot = old.lines.clone();
    let new_snapshot = new.lines.clone();

    for hunk in &mut *hunks {
        let mut i = 0usize;
        while i < hunk.lines.len() {
            if hunk.lines[i].kind != StructuredLineKind::Del {
                i += 1;
                continue;
            }
            let del_start = i;
            while i < hunk.lines.len() && hunk.lines[i].kind == StructuredLineKind::Del {
                i += 1;
            }
            let add_start = i;
            while i < hunk.lines.len() && hunk.lines[i].kind == StructuredLineKind::Add {
                i += 1;
            }
            let del_end = i.min(add_start);
            let add_end = i;
            if del_end > del_start && add_end > add_start {
                let del_count = del_end - del_start;
                let add_count = add_end - add_start;
                let pairs = del_count.min(add_count);
                for p in 0..pairs {
                    let del_idx = del_start + p;
                    let add_idx = add_start + p;
                    let (left, right) = hunk.lines.split_at_mut(add_idx);
                    let del_line = &mut left[del_idx];
                    let add_line = &mut right[0];
                    emphasize_replace_line_pair(del_line, add_line);
                }
            }
        }
    }

    pin_canonical_line_text(hunks, &old_snapshot, &new_snapshot);
}

fn emphasize_replace_line_pair(del: &mut StructuredLine, add: &mut StructuredLine) {
    let old_line = del.segments[0].text.clone();
    let new_line = add.segments[0].text.clone();
    let diff = TextDiff::from_words(&old_line, &new_line);
    del.segments = word_segments_from_diff(&diff, &old_line, true);
    add.segments = word_segments_from_diff(&diff, &new_line, false);
}

fn word_segments_from_diff(
    diff: &TextDiff<'_, '_, str>,
    full_line: &str,
    old_side: bool,
) -> Vec<StructuredSegment> {
    let mut segments: Vec<StructuredSegment> = Vec::new();
    for change in diff.iter_all_changes() {
        let is_this_side = match change.tag() {
            ChangeTag::Delete => old_side,
            ChangeTag::Insert => !old_side,
            ChangeTag::Equal => true,
        };
        if !is_this_side {
            continue;
        }
        let emphasis = change.tag() != ChangeTag::Equal;
        let text = change.value().to_owned();
        if text.is_empty() {
            continue;
        }
        if let Some(last) = segments.last_mut() {
            if last.emphasis == emphasis {
                last.text.push_str(&text);
                continue;
            }
        }
        segments.push(StructuredSegment { text, emphasis });
    }
    if segments.is_empty() && !full_line.is_empty() {
        segments.push(StructuredSegment {
            text: full_line.to_owned(),
            emphasis: false,
        });
    }
    reconcile_segments_with_line(&mut segments, full_line);
    segments
}

/// One O(n) pass: `similar` may drop `\r` in inline segments; restore from precomputed lines.
fn pin_canonical_line_text(
    hunks: &mut [StructuredHunk],
    old_lines: &[String],
    new_lines: &[String],
) {
    for hunk in hunks {
        for line in &mut hunk.lines {
            let canonical = match line.kind {
                StructuredLineKind::Del => line
                    .old
                    .and_then(|n| old_lines.get(n.saturating_sub(1)).cloned()),
                StructuredLineKind::Add => line
                    .new
                    .and_then(|n| new_lines.get(n.saturating_sub(1)).cloned()),
                StructuredLineKind::Context => line
                    .old
                    .and_then(|n| old_lines.get(n.saturating_sub(1)).cloned()),
            };
            let Some(canonical) = canonical else {
                continue;
            };
            if line.kind == StructuredLineKind::Del {
                let emphasis = line.segments.iter().any(|s| s.emphasis);
                line.segments = vec![StructuredSegment {
                    text: canonical,
                    emphasis,
                }];
                continue;
            }
            let rendered: String = line.segments.iter().map(|s| s.text.as_str()).collect();
            if rendered != canonical {
                let emphasis = line.segments.iter().any(|s| s.emphasis);
                line.segments = vec![StructuredSegment {
                    text: canonical,
                    emphasis,
                }];
            }
        }
    }
}

/// Build a structured file diff including mode metadata from a tree diff entry.
#[must_use]
pub fn structured_file_diff(
    old_mode: &str,
    new_mode: &str,
    old: &SideBytes,
    new: &SideBytes,
    options: DiffOptions,
) -> StructuredFileDiff {
    let old_mode = mode_display(old_mode);
    let new_mode = mode_display(new_mode);
    let old_text = side_text_for_diff(old);
    let new_text = side_text_for_diff(new);
    let hunks = structured_text_diff_cow(old_text, new_text, options).hunks;
    StructuredFileDiff {
        old_mode,
        new_mode,
        old_encoding_lossy: old.encoding_lossy,
        new_encoding_lossy: new.encoding_lossy,
        hunks,
    }
}

fn mode_display(mode: &str) -> Option<String> {
    if mode == "000000" {
        None
    } else {
        Some(mode.to_owned())
    }
}

/// Split on `\n` only so `\r` stays inside line bodies (Git CRLF parity).
fn lines_with_trailing_newline(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let ends_nl = text.ends_with('\n');
    let mut parts: Vec<&str> = text.split('\n').collect();
    if ends_nl && parts.last() == Some(&"") {
        parts.pop();
    }
    (parts, ends_nl)
}

fn strip_diff_newline_only(s: &str) -> &str {
    s.strip_suffix('\n').unwrap_or(s)
}

/// Old-side line bodies for structured hunk metadata (CRLF-safe split).
pub(crate) fn old_line_bodies_for_context(text: &str) -> Vec<String> {
    TextLines::from_text(text).lines
}

/// Nearest enclosing definition line above a 1-based old hunk start (Git `-W` style).
pub(crate) fn enclosing_function_context(
    lines: &[String],
    old_start_1based: usize,
) -> Option<String> {
    if old_start_1based == 0 {
        return None;
    }
    function_context(lines, old_start_1based.saturating_sub(1))
}

fn function_context(lines: &[String], start_0based: usize) -> Option<String> {
    lines[..start_0based.min(lines.len())]
        .iter()
        .rev()
        .find_map(|line| {
            let first = line.chars().next()?;
            (first.is_alphabetic() || first == '_').then(|| line.trim_end().to_owned())
        })
}

fn reconcile_segments_with_line(segments: &mut Vec<StructuredSegment>, full_line: &str) {
    if full_line.is_empty() {
        return;
    }
    let rendered: String = segments.iter().map(|s| s.text.as_str()).collect();
    if rendered != full_line {
        let emphasis = segments.iter().any(|s| s.emphasis);
        segments.clear();
        segments.push(StructuredSegment {
            text: full_line.to_owned(),
            emphasis,
        });
    }
}

/// Format one unified hunk range side (`1`, `1,2`, or `0,0`).
#[must_use]
pub fn format_hunk_range(start: usize, count: usize) -> String {
    if count == 0 {
        "0,0".to_owned()
    } else if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn git_diff(old: &str, new: &str) -> String {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, old).expect("write old");
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(tmp.path())
            .status()
            .expect("git init");
        Command::new("git")
            .args(["config", "user.email", "t@e.com"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["config", "user.name", "T"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::fs::write(&path, new).expect("write new");
        let out = Command::new("git")
            .args(["diff", "f.txt"])
            .current_dir(tmp.path())
            .output()
            .expect("git diff");
        String::from_utf8(out.stdout).expect("utf8")
    }

    fn parse_hunk_header(patch: &str) -> (usize, usize, usize, usize) {
        let line = patch.lines().find(|l| l.starts_with("@@")).expect("hunk");
        let rest = line.strip_prefix("@@ ").expect("prefix");
        let (old_part, new_part) = rest.split_once(" +").expect("plus");
        let old_part = old_part.strip_prefix('-').expect("minus");
        let (os, ol) = parse_range(old_part.split(' ').next().expect("old"));
        let new_part = new_part.split(" @@").next().expect("close");
        let (ns, nl) = parse_range(new_part);
        (os, ol, ns, nl)
    }

    fn parse_range(s: &str) -> (usize, usize) {
        if let Some((a, b)) = s.split_once(',') {
            (a.parse().expect("start"), b.parse().expect("count"))
        } else {
            (s.parse().expect("start"), 1)
        }
    }

    #[test]
    fn lines_with_trailing_newline_splits_crlf() {
        let (parts, ends) = lines_with_trailing_newline("a\r\nb\r\n");
        assert!(ends);
        assert_eq!(parts, ["a\r", "b\r"]);
    }

    #[test]
    fn text_lines_from_text_preserves_cr() {
        let t = TextLines::from_text("a\r\nb\r\n");
        assert_eq!(t.lines[0], "a\r");
    }

    #[test]
    fn crlf_to_lf_shows_carriage_return_on_removed_lines() {
        let old = "a\r\nb\r\n";
        let new = "a\nb\n";
        let d = structured_text_diff(old, new, DiffOptions::default());
        let del: Vec<_> = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == StructuredLineKind::Del)
            .collect();
        assert_eq!(del[0].segments[0].text, "a\r");
    }

    #[test]
    fn trailing_newline_only_change_sets_no_newline_flag() {
        let old = "line1\nline2";
        let new = "line1\nline2\n";
        let d = structured_text_diff(old, new, DiffOptions::default());
        let del = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == StructuredLineKind::Del)
            .expect("deleted line2");
        assert!(del.no_newline_at_eof);
    }

    #[test]
    fn context_last_line_without_eof_newline_is_marked() {
        let old = "old\nlast";
        let new = "new\nlast";
        let d = structured_text_diff(old, new, DiffOptions::default());
        let ctx = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == StructuredLineKind::Context && l.segments[0].text == "last")
            .expect("context last");
        assert!(ctx.no_newline_at_eof);
    }

    fn git_diff_u0(old: &str, new: &str) -> String {
        let tmp = tempfile::tempdir().expect("tempdir");
        let path = tmp.path().join("f.txt");
        std::fs::write(&path, old).expect("write old");
        Command::new("git")
            .args(["init", "-q", "-b", "main"])
            .current_dir(tmp.path())
            .status()
            .expect("git init");
        for (k, v) in [("user.email", "t@e.com"), ("user.name", "T")] {
            Command::new("git")
                .args(["config", k, v])
                .current_dir(tmp.path())
                .status()
                .unwrap();
        }
        Command::new("git")
            .args(["add", "f.txt"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        Command::new("git")
            .args(["commit", "-qm", "init"])
            .current_dir(tmp.path())
            .status()
            .unwrap();
        std::fs::write(&path, new).expect("write new");
        let out = Command::new("git")
            .args(["diff", "-U0", "f.txt"])
            .current_dir(tmp.path())
            .output()
            .expect("git diff");
        String::from_utf8(out.stdout).expect("utf8")
    }

    #[test]
    fn zero_context_insertion_anchor_matches_git() {
        let old = "a\nb\nc\n";
        let new = "a\nb\nX\nc\n";
        let patch = git_diff_u0(old, new);
        let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&patch);
        let d = structured_text_diff(
            old,
            new,
            DiffOptions {
                context_lines: 0,
                ..DiffOptions::default()
            },
        );
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (g_os, g_ol, g_ns, g_nl)
        );
        assert_eq!((h.old_start, h.old_lines), (2, 0));
    }

    #[test]
    fn zero_context_deletion_anchor_matches_git() {
        let old = "a\nb\nc\n";
        let new = "a\nc\n";
        let patch = git_diff_u0(old, new);
        let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&patch);
        let d = structured_text_diff(
            old,
            new,
            DiffOptions {
                context_lines: 0,
                ..DiffOptions::default()
            },
        );
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (g_os, g_ol, g_ns, g_nl)
        );
        assert_eq!((h.new_start, h.new_lines), (1, 0));
    }

    #[test]
    fn structured_hunk_retains_enclosing_function_context() {
        let old = "fn foo() {\n  let x = 1;\n}\n";
        let new = "fn foo() {\n  let x = 2;\n}\n";
        let d = structured_text_diff(old, new, DiffOptions::default().with_function_context(true));
        let h = &d.hunks[0];
        assert_eq!(h.context.as_deref(), Some("fn foo() {"));
    }

    #[test]
    fn insert_into_empty_file_matches_git_hunk_header() {
        let patch = git_diff("", "x\n");
        let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&patch);
        let d = structured_text_diff("", "x\n", DiffOptions::default());
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (g_os, g_ol, g_ns, g_nl)
        );
        assert_eq!((h.old_start, h.old_lines), (0, 0));
    }

    #[test]
    fn delete_to_empty_matches_git_hunk_header() {
        let patch = git_diff("x\n", "");
        let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&patch);
        let d = structured_text_diff("x\n", "", DiffOptions::default());
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (g_os, g_ol, g_ns, g_nl)
        );
        assert_eq!((h.new_start, h.new_lines), (0, 0));
    }

    #[test]
    fn line_level_multiple_hunks_match_git() {
        let old = "keep\nchange-a\nmid\nchange-b\nend\n";
        let new = "keep\nCHANGE-A\nmid\nCHANGE-B\nend\n";
        let patch = git_diff_u0(old, new);
        let git_hunks: Vec<_> = patch.lines().filter(|l| l.starts_with("@@")).collect();
        assert_eq!(git_hunks.len(), 2);
        let d = structured_text_diff(
            old,
            new,
            DiffOptions {
                context_lines: 0,
                word_level_inline: false,
                ..DiffOptions::default()
            },
        );
        assert_eq!(d.hunks.len(), 2);
        for (h, header) in d.hunks.iter().zip(git_hunks) {
            let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&format!("{header}\n"));
            assert_eq!(
                (h.old_start, h.old_lines, h.new_start, h.new_lines),
                (g_os, g_ol, g_ns, g_nl)
            );
        }
    }

    #[test]
    fn line_level_blank_line_in_hunk() {
        let old = "a\n\nb\n";
        let new = "a\nX\n\nb\n";
        let d = structured_text_diff(
            old,
            new,
            DiffOptions {
                context_lines: 1,
                word_level_inline: false,
                ..DiffOptions::default()
            },
        );
        assert!(!d.hunks.is_empty());
        assert!(
            d.hunks[0]
                .lines
                .iter()
                .any(|l| l.segments[0].text.is_empty() && l.kind == StructuredLineKind::Context)
                || d.hunks[0]
                    .lines
                    .iter()
                    .any(|l| l.segments[0].text.is_empty())
        );
    }

    #[test]
    fn line_level_missing_eof_newline_flag() {
        let old = "only";
        let new = "only\n";
        let d = structured_text_diff(
            old,
            new,
            DiffOptions {
                context_lines: 0,
                word_level_inline: false,
                ..DiffOptions::default()
            },
        );
        let h = &d.hunks[0];
        let add = h
            .lines
            .iter()
            .find(|l| l.kind == StructuredLineKind::Add)
            .expect("add line");
        assert!(add.no_newline_at_eof || h.lines.iter().any(|l| l.no_newline_at_eof));
    }

    #[test]
    fn hunk_header_counts_match_line_totals() {
        let old = "x\n";
        let new = "x\nmore\n";
        let d = structured_text_diff(
            old,
            new,
            DiffOptions {
                context_lines: 3,
                ..DiffOptions::default()
            },
        );
        let h = &d.hunks[0];
        assert_eq!(h.old_lines, 1);
        assert_eq!(h.new_lines, 2);
    }

    #[test]
    fn mode_only_change_has_empty_hunks() {
        let old = SideBytes::from_bytes(b"x\n".to_vec());
        let new = SideBytes::from_bytes(b"x\n".to_vec());
        let f = structured_file_diff("100644", "100755", &old, &new, DiffOptions::default());
        assert!(f.hunks.is_empty());
    }
}
