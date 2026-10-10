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

/// Build a structured line diff from two decoded text sides.
#[must_use]
pub fn structured_text_diff(
    old_text: &str,
    new_text: &str,
    options: StructuredDiffOptions,
) -> StructuredTextDiff {
    let (old_parts, old_eof_nl) = lines_with_trailing_newline(old_text);
    let (new_parts, new_eof_nl) = lines_with_trailing_newline(new_text);
    let old_line_count = old_parts.len();
    let new_line_count = new_parts.len();

    let diff = TextDiff::from_slices(&old_parts, &new_parts);
    let mut hunks = build_structured_hunks(
        &diff,
        old_text,
        new_text,
        options.context_lines,
        old_eof_nl,
        new_eof_nl,
        old_line_count,
        new_line_count,
    );

    if hunks.is_empty() && old_text != new_text {
        let diff = TextDiff::from_lines(old_text, new_text);
        hunks = build_structured_hunks(
            &diff,
            old_text,
            new_text,
            options.context_lines,
            old_eof_nl,
            new_eof_nl,
            old_line_count,
            new_line_count,
        );
    }

    fix_line_text_from_sources(&mut hunks, old_text, new_text);

    StructuredTextDiff { hunks }
}

fn fix_line_text_from_sources(hunks: &mut [StructuredHunk], old_text: &str, new_text: &str) {
    for hunk in hunks {
        for line in &mut hunk.lines {
            let canonical = match line.kind {
                StructuredLineKind::Del => line
                    .old
                    .map(|n| line_at_text(old_text, Some(n.saturating_sub(1)))),
                StructuredLineKind::Add => line
                    .new
                    .map(|n| line_at_text(new_text, Some(n.saturating_sub(1)))),
                StructuredLineKind::Context => line
                    .old
                    .map(|n| line_at_text(old_text, Some(n.saturating_sub(1)))),
            };
            let Some(canonical) = canonical.filter(|t| !t.is_empty()) else {
                continue;
            };
            let rendered: String = line.segments.iter().map(|s| s.text.as_str()).collect();
            let force = old_text.contains('\r') && line.kind == StructuredLineKind::Del;
            if force || rendered != canonical {
                let emphasis = line.segments.iter().any(|s| s.emphasis);
                line.segments = vec![StructuredSegment {
                    text: canonical,
                    emphasis,
                }];
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_structured_hunks(
    diff: &TextDiff<'_, '_, str>,
    old_text: &str,
    new_text: &str,
    context_lines: usize,
    old_eof_nl: bool,
    new_eof_nl: bool,
    old_line_count: usize,
    new_line_count: usize,
) -> Vec<StructuredHunk> {
    diff.grouped_ops(context_lines)
        .into_iter()
        .filter_map(|group| {
            let first = group.first()?;
            let old_start = first.old_range().start + 1;
            let new_start = first.new_range().start + 1;
            let (old_lines, new_lines) = hunk_line_counts(&group);

            let mut lines = Vec::new();
            for op in &group {
                for change in diff.iter_inline_changes(op) {
                    let kind = match change.tag() {
                        ChangeTag::Equal => StructuredLineKind::Context,
                        ChangeTag::Delete => StructuredLineKind::Del,
                        ChangeTag::Insert => StructuredLineKind::Add,
                    };
                    let source_owned = match kind {
                        StructuredLineKind::Del => line_at_text(old_text, change.old_index()),
                        StructuredLineKind::Add => line_at_text(new_text, change.new_index()),
                        StructuredLineKind::Context => line_at_text(old_text, change.old_index()),
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
                        StructuredLineKind::Del => {
                            line_missing_eof_newline(change.old_index(), old_line_count, old_eof_nl)
                        }
                        StructuredLineKind::Add => {
                            line_missing_eof_newline(change.new_index(), new_line_count, new_eof_nl)
                        }
                        StructuredLineKind::Context => false,
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
                context: function_context(old_text, first.old_range().start),
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

fn line_at_text(text: &str, index: Option<usize>) -> String {
    let (parts, _) = lines_with_trailing_newline(text);
    index
        .and_then(|i| parts.get(i))
        .map(|s| strip_diff_newline_only(s).to_owned())
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

/// Nearest enclosing definition line above `start` (0-based old line index).
fn function_context(old_text: &str, start: usize) -> Option<String> {
    let lines: Vec<&str> = old_text.split('\n').collect();
    let upto = start.min(lines.len());
    lines[..upto].iter().rev().find_map(|line| {
        let first = line.chars().next()?;
        (first.is_alphabetic() || first == '_').then(|| line.trim_end().to_owned())
    })
}

/// Format a unified-style hunk range (`1` or `1,2`).
#[must_use]
pub fn format_hunk_range(start: usize, count: usize) -> String {
    if count <= 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_with_trailing_newline_splits_crlf() {
        let (parts, ends) = lines_with_trailing_newline("a\r\nb\r\n");
        assert!(ends);
        assert_eq!(parts, ["a\r", "b\r"]);
    }

    #[test]
    fn crlf_to_lf_shows_carriage_return_on_removed_lines() {
        let old = "a\r\nb\r\n";
        let new = "a\nb\n";
        let d = structured_text_diff(old, new, StructuredDiffOptions::default());
        assert_eq!(d.hunks.len(), 1);
        let del: Vec<_> = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == StructuredLineKind::Del)
            .collect();
        assert_eq!(del.len(), 2);
        let (parts, _) = lines_with_trailing_newline(old);
        assert_eq!(parts[0], "a\r");
        assert_eq!(del[0].segments[0].text, "a\r");
        assert_eq!(del[1].segments[0].text, "b\r");
        let add: Vec<_> = d.hunks[0]
            .lines
            .iter()
            .filter(|l| l.kind == StructuredLineKind::Add)
            .collect();
        assert_eq!(add[0].segments[0].text, "a");
    }

    #[test]
    fn eof_newline_from_lines_has_ops() {
        let d = TextDiff::from_lines("line1\nline2", "line1\nline2\n");
        assert!(!d.ops().is_empty());
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
        let add = d.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == StructuredLineKind::Add)
            .expect("added line2");
        assert!(!add.no_newline_at_eof);
    }

    #[test]
    fn hunk_header_counts_match_line_totals() {
        let old = "x\n";
        let new = "x\nmore\n";
        let d = structured_text_diff(old, new, StructuredDiffOptions { context_lines: 3 });
        assert_eq!(d.hunks.len(), 1);
        let h = &d.hunks[0];
        assert_eq!(h.old_start, 1);
        assert_eq!(h.new_start, 1);
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
        assert_eq!(f.old_mode.as_deref(), Some("100644"));
        assert_eq!(f.new_mode.as_deref(), Some("100755"));
        assert!(f.hunks.is_empty());
    }

    #[test]
    fn non_utf8_sets_encoding_lossy() {
        let data = vec![b'c', b'a', b'f', 0xe9];
        let side = SideBytes::from_bytes(data);
        assert!(side.encoding_lossy);
    }
}
