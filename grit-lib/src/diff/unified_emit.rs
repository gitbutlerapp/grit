//! Unified diff hunk body emission (independent layout; Git-compatible boundaries).
//!
//! Line breaks in the source text are tracked per logical line so patch lines can
//! preserve CRLF bodies and `\ No newline at end of file` markers match Git.

use crate::diff::core::is_func_line;
use crate::userdiff::FuncnameMatcher;

/// How a source line ended before the next line (or EOF).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
    Missing,
}

/// One logical line of a side, without the line-separator bytes.
#[derive(Clone, Debug)]
pub struct SourceLine {
    pub body: String,
    pub ending: LineEnding,
}

/// All logical lines of one file version.
#[derive(Clone, Debug)]
pub struct LineSide {
    pub lines: Vec<SourceLine>,
}

impl LineSide {
    /// Split `text` into lines, preserving `\r` in bodies and final incomplete lines.
    #[must_use]
    pub fn from_text(text: &str) -> Self {
        if text.is_empty() {
            return Self { lines: Vec::new() };
        }
        let mut lines = Vec::new();
        let mut start = 0usize;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                lines.push(parse_line_segment(&text[start..i], true));
                start = i + 1;
            }
        }
        if start < text.len() {
            lines.push(parse_line_segment(&text[start..], false));
        }
        Self { lines }
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.lines.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }

    pub fn line(&self, index: usize) -> &SourceLine {
        &self.lines[index]
    }

    /// Index of the last line that lacks a trailing line separator, if any.
    #[must_use]
    pub fn incomplete_last_line_index(&self) -> Option<usize> {
        if self.lines.is_empty() {
            return None;
        }
        let last = self.lines.len() - 1;
        (self.lines[last].ending == LineEnding::Missing).then_some(last)
    }
}

fn parse_line_segment(raw: &str, had_line_break: bool) -> SourceLine {
    if had_line_break {
        let ending = if raw.ends_with('\r') {
            LineEnding::CrLf
        } else {
            LineEnding::Lf
        };
        SourceLine {
            body: raw.to_string(),
            ending,
        }
    } else {
        SourceLine {
            body: raw.to_string(),
            ending: LineEnding::Missing,
        }
    }
}

/// When line-based diff finds no edits but the raw texts differ, Git still emits
/// hunks for lines whose bodies match but [`LineEnding`] differs (notably EOF).
#[must_use]
pub fn line_ending_only_edits(before: &LineSide, after: &LineSide) -> Vec<EditSpan> {
    if before.len() != after.len() {
        return Vec::new();
    }
    let mut edits = Vec::new();
    for i in 0..before.len() {
        let bl = before.line(i);
        let al = after.line(i);
        if bl.body != al.body {
            return Vec::new();
        }
        if bl.ending != al.ending {
            edits.push(EditSpan {
                before_start: i as i64,
                before_len: 1,
                after_start: i as i64,
                after_len: 1,
                skip_in_output: false,
            });
        }
    }
    edits
}

/// One contiguous edit between two sides (insert / delete / replace).
#[derive(Clone, Debug)]
pub struct EditSpan {
    pub before_start: i64,
    pub before_len: i64,
    pub after_start: i64,
    pub after_len: i64,
    pub skip_in_output: bool,
}

/// Parameters for hunk grouping and emission.
#[derive(Clone, Copy, Debug)]
pub struct UnifiedEmitOptions {
    pub context_lines: usize,
    pub inter_hunk_context: usize,
    pub function_context: bool,
}

/// Write one patch line (`-`, `+`, or ` `) including Git's no-newline marker when needed.
pub fn write_patch_line(out: &mut String, prefix: char, line: &SourceLine) {
    out.push(prefix);
    out.push_str(&line.body);
    out.push('\n');
    if line.ending == LineEnding::Missing {
        out.push_str("\\ No newline at end of file\n");
    }
}

/// Split on `\n` only when the text has no `\r` (LF-only); lines borrow from `text`.
#[must_use]
pub(crate) fn lf_split_lines(text: &str) -> (Vec<&str>, bool) {
    if text.is_empty() {
        return (Vec::new(), false);
    }
    let ends_with_newline = text.ends_with('\n');
    let mut lines = Vec::new();
    let mut start = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            lines.push(&text[start..i]);
            start = i + 1;
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    (lines, ends_with_newline)
}

fn write_patch_line_lf(
    out: &mut String,
    prefix: char,
    line: &str,
    line_index: usize,
    line_count: usize,
    file_ends_with_newline: bool,
) {
    out.push(prefix);
    out.push_str(line);
    out.push('\n');
    if !file_ends_with_newline && line_index + 1 == line_count {
        out.push_str("\\ No newline at end of file\n");
    }
}

/// Like [`line_ending_only_edits`] for LF-only sides from [`lf_split_lines`].
pub(crate) fn line_ending_only_edits_lf(
    before: &[&str],
    before_ends_nl: bool,
    after: &[&str],
    after_ends_nl: bool,
) -> Vec<EditSpan> {
    if before.len() != after.len() {
        return Vec::new();
    }
    let mut edits = Vec::new();
    for i in 0..before.len() {
        if before[i] != after[i] {
            return Vec::new();
        }
        let before_missing = !before_ends_nl && i + 1 == before.len();
        let after_missing = !after_ends_nl && i + 1 == after.len();
        if before_missing != after_missing {
            edits.push(EditSpan {
                before_start: i as i64,
                before_len: 1,
                after_start: i as i64,
                after_len: 1,
                skip_in_output: false,
            });
        }
    }
    edits
}

/// Emit unified hunks when both sides are LF-only (no per-line `String` allocation).
pub(crate) fn emit_unified_body_from_edits_lf(
    before_lines: &[&str],
    before_ends_nl: bool,
    after_lines: &[&str],
    after_ends_nl: bool,
    edits: &[EditSpan],
    options: UnifiedEmitOptions,
    funcname_matcher: Option<&FuncnameMatcher>,
) -> String {
    use std::fmt::Write as _;

    if edits.is_empty() {
        return String::new();
    }
    let before_len = before_lines.len() as i64;
    let after_len = after_lines.len() as i64;
    let ctxlen = options.context_lines as i64;
    let interhunk = options.inter_hunk_context as i64;
    let max_common = ctxlen.saturating_add(ctxlen).saturating_add(interhunk);
    let max_ignorable = ctxlen;

    let before_body = |i: i64| -> &str {
        if i < 0 || i as usize >= before_lines.len() {
            return "";
        }
        before_lines[i as usize]
    };
    let next_hunk = |start: usize| -> Option<(usize, usize)> {
        let mut scr = start;
        let mut xchp = start;
        while xchp < edits.len() && edits[xchp].skip_in_output {
            let next = xchp + 1;
            if next >= edits.len()
                || edits[next].before_start - (edits[xchp].before_start + edits[xchp].before_len)
                    >= max_ignorable
            {
                scr = next;
            }
            xchp = next;
        }
        if scr >= edits.len() {
            return None;
        }

        let mut lx = scr;
        let mut ignored_after: i64 = 0;
        let mut prev = scr;
        let mut idx = scr + 1;
        while idx < edits.len() {
            let cur = &edits[idx];
            let prev_e = &edits[prev];
            let distance = cur.before_start - (prev_e.before_start + prev_e.before_len);
            if distance > max_common {
                break;
            }
            if distance < max_ignorable && (!cur.skip_in_output || lx == prev) {
                lx = idx;
                ignored_after = 0;
            } else if distance < max_ignorable && cur.skip_in_output {
                ignored_after += cur.after_len;
            } else if lx != prev
                && cur.before_start + ignored_after
                    - (edits[lx].before_start + edits[lx].before_len)
                    > max_common
            {
                break;
            } else if !cur.skip_in_output {
                lx = idx;
                ignored_after = 0;
            } else {
                ignored_after += cur.after_len;
            }
            prev = idx;
            idx += 1;
        }
        Some((scr, lx))
    };

    let mut out = String::new();
    let mut cursor = 0usize;
    while cursor < edits.len() {
        let Some((first_idx, last_idx)) = next_hunk(cursor) else {
            break;
        };
        let first = &edits[first_idx];
        let last = &edits[last_idx];

        let mut b_start = (first.before_start - ctxlen).max(0);
        let mut a_start = (first.after_start - ctxlen).max(0);
        let mut post = ctxlen;
        post = post.min(before_len - (last.before_start + last.before_len));
        post = post.min(after_len - (last.after_start + last.after_len));
        let mut b_end = last.before_start + last.before_len + post;
        let mut a_end = last.after_start + last.after_len + post;

        if options.function_context {
            let mut func_start = first.before_start;
            while func_start >= 0 && !is_func_line(before_body(func_start), funcname_matcher) {
                func_start -= 1;
            }
            while func_start > 0
                && !before_body(func_start - 1).trim().is_empty()
                && !is_func_line(before_body(func_start - 1), funcname_matcher)
            {
                func_start -= 1;
            }
            if func_start < 0 {
                func_start = 0;
            }
            if func_start < b_start {
                a_start = (a_start - (b_start - func_start)).max(0);
                b_start = func_start;
            }

            let end_before = last.before_start + last.before_len;
            let mut func_end = end_before;
            while func_end < before_len && !is_func_line(before_body(func_end), funcname_matcher) {
                func_end += 1;
            }
            while func_end > 0 && before_body(func_end - 1).trim().is_empty() {
                func_end -= 1;
            }
            if func_end > before_len {
                func_end = before_len;
            }
            if func_end > b_end {
                a_end = (a_end + (func_end - b_end)).min(after_len);
                b_end = func_end;
            }
        }

        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            format_hunk_side(b_start, b_end - b_start),
            format_hunk_side(a_start, a_end - a_start)
        );

        let mut a_cur = a_start;
        while a_cur < first.after_start {
            let idx = a_cur as usize;
            write_patch_line_lf(
                &mut out,
                ' ',
                after_lines[idx],
                idx,
                after_lines.len(),
                after_ends_nl,
            );
            a_cur += 1;
        }

        let mut b_cur = first.before_start;
        let mut a_cur = first.after_start;
        let mut k = first_idx;
        loop {
            let e = &edits[k];
            while b_cur < e.before_start && a_cur < e.after_start {
                let idx = a_cur as usize;
                write_patch_line_lf(
                    &mut out,
                    ' ',
                    after_lines[idx],
                    idx,
                    after_lines.len(),
                    after_ends_nl,
                );
                b_cur += 1;
                a_cur += 1;
            }
            let mut r = e.before_start;
            while r < e.before_start + e.before_len {
                let idx = r as usize;
                write_patch_line_lf(
                    &mut out,
                    '-',
                    before_lines[idx],
                    idx,
                    before_lines.len(),
                    before_ends_nl,
                );
                r += 1;
            }
            let mut a = e.after_start;
            while a < e.after_start + e.after_len {
                let idx = a as usize;
                write_patch_line_lf(
                    &mut out,
                    '+',
                    after_lines[idx],
                    idx,
                    after_lines.len(),
                    after_ends_nl,
                );
                a += 1;
            }
            if k == last_idx {
                break;
            }
            b_cur = e.before_start + e.before_len;
            a_cur = e.after_start + e.after_len;
            k += 1;
        }

        let mut a_tail = last.after_start + last.after_len;
        while a_tail < a_end {
            let idx = a_tail as usize;
            write_patch_line_lf(
                &mut out,
                ' ',
                after_lines[idx],
                idx,
                after_lines.len(),
                after_ends_nl,
            );
            a_tail += 1;
        }

        cursor = last_idx + 1;
    }

    out
}

fn format_hunk_side(start0: i64, count: i64) -> String {
    let shown_start = if count == 0 { start0 } else { start0 + 1 };
    if count == 1 {
        format!("{shown_start}")
    } else {
        format!("{shown_start},{count}")
    }
}

/// Emit unified hunk bodies for precomputed edit spans (Myers / xdiff ops path).
#[allow(clippy::too_many_arguments)]
pub fn emit_unified_body_from_edits(
    before: &LineSide,
    after: &LineSide,
    edits: &[EditSpan],
    options: UnifiedEmitOptions,
    funcname_matcher: Option<&FuncnameMatcher>,
) -> String {
    use std::fmt::Write as _;

    if edits.is_empty() {
        return String::new();
    }
    let before_len = before.len() as i64;
    let after_len = after.len() as i64;
    let ctxlen = options.context_lines as i64;
    let interhunk = options.inter_hunk_context as i64;
    let max_common = ctxlen.saturating_add(ctxlen).saturating_add(interhunk);
    let max_ignorable = ctxlen;

    let before_body = |i: i64| -> &str {
        if i < 0 || i as usize >= before.lines.len() {
            return "";
        }
        &before.lines[i as usize].body
    };
    let next_hunk = |start: usize| -> Option<(usize, usize)> {
        let mut scr = start;
        let mut xchp = start;
        while xchp < edits.len() && edits[xchp].skip_in_output {
            let next = xchp + 1;
            if next >= edits.len()
                || edits[next].before_start - (edits[xchp].before_start + edits[xchp].before_len)
                    >= max_ignorable
            {
                scr = next;
            }
            xchp = next;
        }
        if scr >= edits.len() {
            return None;
        }

        let mut lx = scr;
        let mut ignored_after: i64 = 0;
        let mut prev = scr;
        let mut idx = scr + 1;
        while idx < edits.len() {
            let cur = &edits[idx];
            let prev_e = &edits[prev];
            let distance = cur.before_start - (prev_e.before_start + prev_e.before_len);
            if distance > max_common {
                break;
            }
            if distance < max_ignorable && (!cur.skip_in_output || lx == prev) {
                lx = idx;
                ignored_after = 0;
            } else if distance < max_ignorable && cur.skip_in_output {
                ignored_after += cur.after_len;
            } else if lx != prev
                && cur.before_start + ignored_after
                    - (edits[lx].before_start + edits[lx].before_len)
                    > max_common
            {
                break;
            } else if !cur.skip_in_output {
                lx = idx;
                ignored_after = 0;
            } else {
                ignored_after += cur.after_len;
            }
            prev = idx;
            idx += 1;
        }
        Some((scr, lx))
    };

    let mut out = String::new();
    let mut cursor = 0usize;
    while cursor < edits.len() {
        let Some((first_idx, last_idx)) = next_hunk(cursor) else {
            break;
        };
        let first = &edits[first_idx];
        let last = &edits[last_idx];

        let mut b_start = (first.before_start - ctxlen).max(0);
        let mut a_start = (first.after_start - ctxlen).max(0);
        let mut post = ctxlen;
        post = post.min(before_len - (last.before_start + last.before_len));
        post = post.min(after_len - (last.after_start + last.after_len));
        let mut b_end = last.before_start + last.before_len + post;
        let mut a_end = last.after_start + last.after_len + post;

        if options.function_context {
            let mut func_start = first.before_start;
            while func_start >= 0 && !is_func_line(before_body(func_start), funcname_matcher) {
                func_start -= 1;
            }
            while func_start > 0
                && !before_body(func_start - 1).trim().is_empty()
                && !is_func_line(before_body(func_start - 1), funcname_matcher)
            {
                func_start -= 1;
            }
            if func_start < 0 {
                func_start = 0;
            }
            if func_start < b_start {
                a_start = (a_start - (b_start - func_start)).max(0);
                b_start = func_start;
            }

            let end_before = last.before_start + last.before_len;
            let mut func_end = end_before;
            while func_end < before_len && !is_func_line(before_body(func_end), funcname_matcher) {
                func_end += 1;
            }
            while func_end > 0 && before_body(func_end - 1).trim().is_empty() {
                func_end -= 1;
            }
            if func_end > before_len {
                func_end = before_len;
            }
            if func_end > b_end {
                a_end = (a_end + (func_end - b_end)).min(after_len);
                b_end = func_end;
            }
        }

        let _ = writeln!(
            out,
            "@@ -{} +{} @@",
            format_hunk_side(b_start, b_end - b_start),
            format_hunk_side(a_start, a_end - a_start)
        );

        let mut a_cur = a_start;
        while a_cur < first.after_start {
            write_patch_line(&mut out, ' ', after.line(a_cur as usize));
            a_cur += 1;
        }

        let mut b_cur = first.before_start;
        let mut a_cur = first.after_start;
        let mut k = first_idx;
        loop {
            let e = &edits[k];
            while b_cur < e.before_start && a_cur < e.after_start {
                write_patch_line(&mut out, ' ', after.line(a_cur as usize));
                b_cur += 1;
                a_cur += 1;
            }
            let mut r = e.before_start;
            while r < e.before_start + e.before_len {
                write_patch_line(&mut out, '-', before.line(r as usize));
                r += 1;
            }
            let mut a = e.after_start;
            while a < e.after_start + e.after_len {
                write_patch_line(&mut out, '+', after.line(a as usize));
                a += 1;
            }
            if k == last_idx {
                break;
            }
            b_cur = e.before_start + e.before_len;
            a_cur = e.after_start + e.after_len;
            k += 1;
        }

        let mut a_tail = last.after_start + last.after_len;
        while a_tail < a_end {
            write_patch_line(&mut out, ' ', after.line(a_tail as usize));
            a_tail += 1;
        }

        cursor = last_idx + 1;
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_preserves_crlf_in_body() {
        let side = LineSide::from_text("a\r\nb");
        assert_eq!(side.lines.len(), 2);
        assert_eq!(side.lines[0].body, "a\r");
        assert_eq!(side.lines[0].ending, LineEnding::CrLf);
        assert_eq!(side.lines[1].body, "b");
        assert_eq!(side.lines[1].ending, LineEnding::Missing);
    }

    #[test]
    fn patch_line_marks_missing_newline() {
        let line = SourceLine {
            body: "x".into(),
            ending: LineEnding::Missing,
        };
        let mut out = String::new();
        write_patch_line(&mut out, '-', &line);
        assert!(out.contains("\\ No newline at end of file"));
    }

    #[test]
    fn eof_newline_only_change_yields_replace_hunk() {
        let before = LineSide::from_text("one");
        let after = LineSide::from_text("one\n");
        let edits = line_ending_only_edits(&before, &after);
        assert_eq!(edits.len(), 1);
        let body = emit_unified_body_from_edits(
            &before,
            &after,
            &edits,
            UnifiedEmitOptions {
                context_lines: 3,
                inter_hunk_context: 0,
                function_context: false,
            },
            None,
        );
        assert!(body.contains("-one\n"));
        assert!(body.contains("+one\n"));
        assert!(body.contains("\\ No newline at end of file"));
    }
}
