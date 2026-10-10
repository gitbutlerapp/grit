//! Structured line diffs for CLI and embedders (`grit diff`, `grit show`).
//!
//! Preserves line-ending details (CRLF vs LF, missing newline at EOF), mode-only
//! changes, hunk line counts, and optional UTF-8 lossy decoding markers — without
//! relying on unified patch text parsing.

use similar::{ChangeTag, DiffOp, TextDiff};

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

/// Options for [`structured_text_diff`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StructuredDiffOptions {
    /// Unchanged context lines around each change (Git `-U`).
    pub context_lines: usize,
}

impl Default for StructuredDiffOptions {
    fn default() -> Self {
        Self { context_lines: 3 }
    }
}

/// One run of text within a line, with optional intra-line emphasis.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredSegment {
    pub text: String,
    pub emphasis: bool,
}

/// Kind of line in a structured hunk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuredLineKind {
    Context,
    Add,
    Del,
}

/// One rendered line in a structured hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredLine {
    pub kind: StructuredLineKind,
    pub old: Option<usize>,
    pub new: Option<usize>,
    pub segments: Vec<StructuredSegment>,
    /// Git `\ No newline at end of file` on this side.
    pub no_newline_at_eof: bool,
}

/// A contiguous run of changes plus surrounding context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredHunk {
    pub old_start: usize,
    pub new_start: usize,
    pub old_lines: usize,
    pub new_lines: usize,
    pub context: Option<String>,
    pub lines: Vec<StructuredLine>,
}

/// Line-level diff of two text sides (no file metadata).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredTextDiff {
    pub hunks: Vec<StructuredHunk>,
}

/// File metadata plus a structured line diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredFileDiff {
    pub old_mode: Option<String>,
    pub new_mode: Option<String>,
    pub old_encoding_lossy: bool,
    pub new_encoding_lossy: bool,
    pub hunks: Vec<StructuredHunk>,
}

/// Pre-split line bodies for O(1) lookup while building hunks.
#[derive(Debug, Clone)]
struct TextLines {
    lines: Vec<String>,
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

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn slice_refs(&self) -> Vec<&str> {
        self.lines.iter().map(String::as_str).collect()
    }
}

/// Build a structured line diff from two decoded text sides.
#[must_use]
pub fn structured_text_diff(
    old_text: &str,
    new_text: &str,
    options: StructuredDiffOptions,
) -> StructuredTextDiff {
    let old = TextLines::from_text(old_text);
    let new = TextLines::from_text(new_text);
    let old_snapshot = old.lines.clone();
    let new_snapshot = new.lines.clone();

    let diff = TextDiff::from_slices(&old.slice_refs(), &new.slice_refs());
    let mut hunks = build_structured_hunks(
        &diff,
        &old,
        &new,
        &old_snapshot,
        &new_snapshot,
        options.context_lines,
    );

    if hunks.is_empty() && old_text != new_text {
        let diff = TextDiff::from_lines(old_text, new_text);
        hunks = build_structured_hunks(
            &diff,
            &old,
            &new,
            &old_snapshot,
            &new_snapshot,
            options.context_lines,
        );
    }

    pin_canonical_line_text(&mut hunks, &old_snapshot, &new_snapshot);

    StructuredTextDiff { hunks }
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

fn build_structured_hunks(
    diff: &TextDiff<'_, '_, str>,
    old: &TextLines,
    new: &TextLines,
    old_bodies: &[String],
    new_bodies: &[String],
    context_lines: usize,
) -> Vec<StructuredHunk> {
    diff.grouped_ops(context_lines)
        .into_iter()
        .filter_map(|group| {
            let first = group.first()?;
            let (old_lines, new_lines) = hunk_line_counts(&group);
            let old_start = unified_side_start(first.old_range().start, old_lines);
            let new_start = unified_side_start(first.new_range().start, new_lines);

            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_inline_changes(op) {
                    let kind = match change.tag() {
                        ChangeTag::Equal => StructuredLineKind::Context,
                        ChangeTag::Delete => StructuredLineKind::Del,
                        ChangeTag::Insert => StructuredLineKind::Add,
                    };
                    let source_owned = match kind {
                        StructuredLineKind::Del => line_body(old_bodies, change.old_index()),
                        StructuredLineKind::Add => line_body(new_bodies, change.new_index()),
                        StructuredLineKind::Context => line_body(old_bodies, change.old_index()),
                    };
                    let inline = segments_for_change(&change, &source_owned);
                    let rendered: String = inline.iter().map(|s| s.text.as_str()).collect();
                    let segments = if source_owned.is_empty() || rendered == source_owned {
                        inline
                    } else {
                        vec![StructuredSegment {
                            text: source_owned,
                            emphasis: inline.iter().any(|s| s.emphasis),
                        }]
                    };

                    let no_newline_at_eof = match kind {
                        StructuredLineKind::Del => line_missing_eof_newline(
                            change.old_index(),
                            old.line_count(),
                            old.ends_with_newline,
                        ),
                        StructuredLineKind::Add => line_missing_eof_newline(
                            change.new_index(),
                            new.line_count(),
                            new.ends_with_newline,
                        ),
                        StructuredLineKind::Context => {
                            line_missing_eof_newline(
                                change.old_index(),
                                old.line_count(),
                                old.ends_with_newline,
                            ) || line_missing_eof_newline(
                                change.new_index(),
                                new.line_count(),
                                new.ends_with_newline,
                            )
                        }
                    };

                    lines.push(StructuredLine {
                        kind,
                        old: change.old_index().map(|i| i + 1),
                        new: change.new_index().map(|i| i + 1),
                        segments,
                        no_newline_at_eof,
                    });
                }
            }

            Some(StructuredHunk {
                old_start,
                new_start,
                old_lines,
                new_lines,
                context: function_context(&old.lines, first.old_range().start),
                lines,
            })
        })
        .collect()
}

/// Build a structured file diff including mode metadata from a tree diff entry.
#[must_use]
pub fn structured_file_diff(
    old_mode: &str,
    new_mode: &str,
    old: &SideBytes,
    new: &SideBytes,
    options: StructuredDiffOptions,
) -> StructuredFileDiff {
    let old_mode = mode_display(old_mode);
    let new_mode = mode_display(new_mode);
    let body = structured_text_diff(&old.text(), &new.text(), options);
    StructuredFileDiff {
        old_mode,
        new_mode,
        old_encoding_lossy: old.encoding_lossy,
        new_encoding_lossy: new.encoding_lossy,
        hunks: body.hunks,
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

fn line_body(lines: &[String], index: Option<usize>) -> String {
    index
        .and_then(|i| lines.get(i))
        .cloned()
        .unwrap_or_default()
}

fn segments_for_change(
    change: &similar::InlineChange<'_, str>,
    full_line: &str,
) -> Vec<StructuredSegment> {
    let full_line = strip_diff_newline_only(full_line);
    debug_assert!(
        !full_line.contains('\n'),
        "line text must not contain newline delimiters"
    );
    let inline: Vec<(bool, String)> = change
        .iter_strings_lossy()
        .map(|(emphasis, value)| (emphasis, strip_diff_newline_only(value.as_ref()).to_owned()))
        .filter(|(_, text)| !text.is_empty())
        .collect();
    if inline.is_empty() {
        if full_line.is_empty() {
            return Vec::new();
        }
        return vec![StructuredSegment {
            text: full_line.to_owned(),
            emphasis: false,
        }];
    }

    if inline.len() == 1 {
        let (emphasis, _) = inline[0];
        let mut one = vec![StructuredSegment {
            text: full_line.to_owned(),
            emphasis,
        }];
        reconcile_segments_with_line(&mut one, full_line);
        return one;
    }

    let joined: String = inline.iter().map(|(_, t)| t.as_str()).collect();
    if joined != full_line {
        let trailing = full_line.strip_prefix(&joined).unwrap_or("");
        let mut out: Vec<StructuredSegment> = inline
            .into_iter()
            .map(|(emphasis, text)| StructuredSegment { text, emphasis })
            .collect();
        if !trailing.is_empty() {
            if let Some(last) = out.last_mut() {
                last.text.push_str(trailing);
            } else {
                out.push(StructuredSegment {
                    text: trailing.to_owned(),
                    emphasis: false,
                });
            }
        }
        reconcile_segments_with_line(&mut out, full_line);
        return out;
    }

    let mut out: Vec<StructuredSegment> = inline
        .into_iter()
        .map(|(emphasis, text)| StructuredSegment { text, emphasis })
        .collect();
    reconcile_segments_with_line(&mut out, full_line);
    out
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

fn line_missing_eof_newline(
    index: Option<usize>,
    line_count: usize,
    file_ends_with_newline: bool,
) -> bool {
    if file_ends_with_newline {
        return false;
    }
    index == Some(line_count.saturating_sub(1))
}

fn hunk_line_counts(group: &[DiffOp]) -> (usize, usize) {
    let mut old = 0usize;
    let mut new = 0usize;
    for op in group {
        match *op {
            DiffOp::Equal { len, .. } => {
                old += len;
                new += len;
            }
            DiffOp::Delete { old_len, .. } => old += old_len,
            DiffOp::Insert { new_len, .. } => new += new_len,
            DiffOp::Replace {
                old_len, new_len, ..
            } => {
                old += old_len;
                new += new_len;
            }
        }
    }
    (old, new)
}

/// Git unified hunk start for one side (`xdiff` / `git diff`).
///
/// When the side has lines in the hunk, start is 1-based. When the count is zero,
/// start is the 0-based anchor from the diff op (0 only for an empty file).
fn unified_side_start(anchor_index_0based: usize, line_count: usize) -> usize {
    if line_count == 0 {
        anchor_index_0based
    } else {
        anchor_index_0based + 1
    }
}

/// Nearest enclosing definition line above `start` (0-based old line index).
fn function_context(lines: &[String], start: usize) -> Option<String> {
    lines[..start.min(lines.len())]
        .iter()
        .rev()
        .find_map(|line| {
            let first = line.chars().next()?;
            (first.is_alphabetic() || first == '_').then(|| line.trim_end().to_owned())
        })
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
        let d = structured_text_diff(old, new, StructuredDiffOptions::default());
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
        let d = structured_text_diff(old, new, StructuredDiffOptions::default());
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
        let d = structured_text_diff(old, new, StructuredDiffOptions::default());
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
        let d = structured_text_diff(old, new, StructuredDiffOptions { context_lines: 0 });
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
        let d = structured_text_diff(old, new, StructuredDiffOptions { context_lines: 0 });
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (g_os, g_ol, g_ns, g_nl)
        );
        assert_eq!((h.new_start, h.new_lines), (1, 0));
    }

    #[test]
    fn insert_into_empty_file_matches_git_hunk_header() {
        let patch = git_diff("", "x\n");
        let (g_os, g_ol, g_ns, g_nl) = parse_hunk_header(&patch);
        let d = structured_text_diff("", "x\n", StructuredDiffOptions::default());
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
        let d = structured_text_diff("x\n", "", StructuredDiffOptions::default());
        let h = &d.hunks[0];
        assert_eq!(
            (h.old_start, h.old_lines, h.new_start, h.new_lines),
            (g_os, g_ol, g_ns, g_nl)
        );
        assert_eq!((h.new_start, h.new_lines), (0, 0));
    }

    #[test]
    fn hunk_header_counts_match_line_totals() {
        let old = "x\n";
        let new = "x\nmore\n";
        let d = structured_text_diff(old, new, StructuredDiffOptions { context_lines: 3 });
        let h = &d.hunks[0];
        assert_eq!(h.old_lines, 1);
        assert_eq!(h.new_lines, 2);
    }

    #[test]
    fn mode_only_change_has_empty_hunks() {
        let old = SideBytes::from_bytes(b"x\n".to_vec());
        let new = SideBytes::from_bytes(b"x\n".to_vec());
        let f = structured_file_diff(
            "100644",
            "100755",
            &old,
            &new,
            StructuredDiffOptions::default(),
        );
        assert!(f.hunks.is_empty());
    }
}
