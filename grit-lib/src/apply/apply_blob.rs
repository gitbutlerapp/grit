//! Apply parsed unified-diff hunks to a single blob (no worktree or index I/O).

use std::borrow::Cow;

use super::{Hunk, HunkLine};
use crate::error::ApplyError;

/// Options controlling blob-level hunk application.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ApplyOptions {
    /// When true, interpret the patch as reversing a forward diff (`+`/`-` swapped).
    pub reverse: bool,
}

/// One logical line in the working blob (bytes include `\n` when the source line had one).
///
/// Untouched rows borrow from the original blob; only rows written by a hunk are owned.
#[derive(Debug, Clone)]
struct LineRecord<'a> {
    raw: Cow<'a, [u8]>,
    /// Set when an earlier hunk replaced this row (later hunks must not match through it).
    consumed: bool,
}

/// Mutable line-split view of blob bytes while hunks are applied in order.
#[derive(Debug, Clone)]
struct WorkingBlob<'a> {
    rows: Vec<LineRecord<'a>>,
}

impl<'a> WorkingBlob<'a> {
    fn decode(data: &'a [u8]) -> Self {
        let mut rows = Vec::with_capacity(memchr::memchr_iter(b'\n', data).count() + 1);
        let mut start = 0usize;
        for i in memchr::memchr_iter(b'\n', data) {
            rows.push(LineRecord {
                raw: Cow::Borrowed(&data[start..=i]),
                consumed: false,
            });
            start = i + 1;
        }
        if start < data.len() {
            rows.push(LineRecord {
                raw: Cow::Borrowed(&data[start..]),
                consumed: false,
            });
        }
        Self { rows }
    }

    fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.rows.iter().map(|row| row.raw.len()).sum());
        for row in &self.rows {
            out.extend_from_slice(&row.raw);
        }
        out
    }

    fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Lines that must match in the working blob vs lines that replace them.
#[derive(Debug, Clone)]
struct LineEdit {
    match_rows: Vec<Vec<u8>>,
    replace_rows: Vec<Vec<u8>>,
    stable_prefix: usize,
    stable_suffix: usize,
}

/// Where to start scanning for [`LineEdit::match_rows`].
#[derive(Debug, Clone, Copy)]
struct ScanHint {
    /// 0-based row index in the current working blob.
    start_row: usize,
    /// Match only when the edit aligns with the first row of the blob.
    only_at_front: bool,
    /// Match only when the edit ends on the last row of the blob.
    only_at_back: bool,
}

/// Apply `hunks` to `original` and return the post-image bytes.
///
/// Matches `git apply` default behavior: exact line match at the hunk's post-image
/// line (`new_start` forward, `old_start` when reversing), then search outward;
/// prefix/suffix context rows may be dropped and the search retried. No fuzz or
/// whitespace fixing.
pub fn apply_hunks_to_blob(
    original: &[u8],
    hunks: &[Hunk],
    opts: &ApplyOptions,
) -> Result<Vec<u8>, ApplyError> {
    let mut working = WorkingBlob::decode(original);
    for (hunk_index, hunk) in hunks.iter().enumerate() {
        let edit = line_edit_from_hunk(hunk, opts.reverse);
        let insert_only = if opts.reverse {
            hunk.new_count == 0
        } else {
            hunk.old_count == 0
        };
        let hint = scan_hint_for_hunk(hunk, &edit, opts.reverse);
        let Some(at) = locate_and_apply(&mut working, &edit, hint, hunk, hunk_index, insert_only)?
        else {
            return Err(ApplyError::ContextMismatch {
                hunk_index,
                expected_line: if opts.reverse {
                    hunk.old_start
                } else {
                    hunk.new_start
                },
            });
        };
        let _ = at;
    }
    Ok(working.encode())
}

fn scan_hint_for_hunk(hunk: &Hunk, edit: &LineEdit, reverse: bool) -> ScanHint {
    let (header_line, only_at_front) = if reverse {
        (hunk.old_start, hunk.new_start <= 1)
    } else {
        (hunk.new_start, hunk.old_start <= 1)
    };
    let start_row = if header_line == 0 {
        0
    } else {
        header_line.saturating_sub(1)
    };
    ScanHint {
        start_row,
        only_at_front,
        // Pin to EOF only when the hunk had leading context but no trailing context
        // (a change that ends at the file end). Zero-context hunks (`-U0`) must not
        // take this pin unless we add an explicit unidiff-zero mode; otherwise a line
        // that also appears at EOF is matched at the wrong row.
        only_at_back: edit.stable_suffix == 0 && edit.stable_prefix > 0,
    }
}

fn line_edit_from_hunk(hunk: &Hunk, reverse: bool) -> LineEdit {
    let mut match_rows = Vec::new();
    let mut replace_rows = Vec::new();
    let mut stable_prefix = 0usize;
    let mut stable_suffix = 0usize;
    let mut in_change = false;

    let mut i = 0usize;
    while i < hunk.lines.len() {
        let (tag, text) = match &hunk.lines[i] {
            HunkLine::Context(s) => ("ctx", s.as_str()),
            HunkLine::Add(s) => ("add", s.as_str()),
            HunkLine::Remove(s) => ("del", s.as_str()),
            HunkLine::NoNewline => {
                i += 1;
                continue;
            }
        };
        let tag = if reverse {
            match tag {
                "add" => "del",
                "del" => "add",
                other => other,
            }
        } else {
            tag
        };

        let with_nl = !matches!(hunk.lines.get(i + 1), Some(HunkLine::NoNewline));
        let row = hunk_text_as_row(text, with_nl);

        match tag {
            "ctx" => {
                if !in_change {
                    stable_prefix += 1;
                }
                stable_suffix += 1;
                match_rows.push(row.clone());
                replace_rows.push(row);
            }
            "del" => {
                in_change = true;
                stable_suffix = 0;
                match_rows.push(row);
            }
            "add" => {
                in_change = true;
                stable_suffix = 0;
                replace_rows.push(row);
            }
            _ => {}
        }
        i += 1;
    }

    if !in_change {
        stable_suffix = stable_prefix;
    }

    LineEdit {
        match_rows,
        replace_rows,
        stable_prefix,
        stable_suffix,
    }
}

fn hunk_text_as_row(text: &str, with_newline: bool) -> Vec<u8> {
    let mut row = text.as_bytes().to_vec();
    if with_newline {
        row.push(b'\n');
    }
    row
}

fn locate_and_apply(
    working: &mut WorkingBlob,
    edit: &LineEdit,
    hint: ScanHint,
    hunk: &Hunk,
    _hunk_index: usize,
    insert_only: bool,
) -> Result<Option<usize>, ApplyError> {
    let mut candidate = edit.clone();
    let mut hint = hint;
    let mut prefix = edit.stable_prefix;
    let mut suffix = edit.stable_suffix;
    let mut pin_front = hint.only_at_front;
    let mut pin_back = hint.only_at_back;

    loop {
        if candidate.match_rows.is_empty() {
            if insert_only && !candidate.replace_rows.is_empty() {
                let row = insert_only_row(hint, working, hunk);
                splice_edit(working, row, &candidate);
                return Ok(Some(row));
            }
            if prefix == 0 && suffix == 0 {
                return Ok(None);
            }
        } else if let Some(row) =
            search_outward(working, &candidate.match_rows, hint, pin_front, pin_back)
        {
            splice_edit(working, row, &candidate);
            return Ok(Some(row));
        }

        if pin_front || pin_back {
            pin_front = false;
            pin_back = false;
            continue;
        }
        if prefix == 0 && suffix == 0 {
            return Ok(None);
        }
        if prefix >= suffix {
            trim_edit_front(&mut candidate);
            prefix -= 1;
            hint.start_row = hint.start_row.saturating_sub(1);
        }
        if suffix > prefix {
            trim_edit_back(&mut candidate);
            suffix -= 1;
        }
    }
}

fn trim_edit_front(edit: &mut LineEdit) {
    if !edit.match_rows.is_empty() {
        edit.match_rows.remove(0);
    }
    if !edit.replace_rows.is_empty() {
        edit.replace_rows.remove(0);
    }
}

fn trim_edit_back(edit: &mut LineEdit) {
    if !edit.match_rows.is_empty() {
        edit.match_rows.pop();
    }
    if !edit.replace_rows.is_empty() {
        edit.replace_rows.pop();
    }
}

fn insert_only_row(hint: ScanHint, working: &WorkingBlob, hunk: &Hunk) -> usize {
    let len = working.len();
    let at = hint.start_row.min(len);
    if hint.only_at_back
        && hunk.old_count == 0
        && hunk.old_start > 0
        && hunk.old_start >= len
        && hunk.new_start >= hunk.old_start
    {
        return len;
    }
    at
}

fn search_outward(
    working: &WorkingBlob,
    pattern: &[Vec<u8>],
    hint: ScanHint,
    pin_front: bool,
    pin_back: bool,
) -> Option<usize> {
    let width = pattern.len();
    debug_assert!(!pattern.is_empty());
    let mut anchor = hint.start_row.min(working.len());
    if pin_front {
        anchor = 0;
    } else if pin_back {
        anchor = working.len().saturating_sub(width);
    }

    let mut low = anchor;
    let mut high = anchor;
    let mut cursor = anchor;

    let attempts = working.len().saturating_add(width).saturating_add(1);
    for step in 0..attempts {
        if rows_equal_pattern(working, cursor, pattern, pin_front, pin_back) {
            return Some(cursor);
        }

        let can_low = low > 0;
        let can_high = high < working.len();
        if !can_low && !can_high {
            break;
        }

        if step & 1 == 1 {
            if can_low {
                low -= 1;
                cursor = low;
            }
        } else if can_high {
            high += 1;
            cursor = high;
        }
    }
    None
}

fn rows_equal_pattern(
    working: &WorkingBlob,
    at: usize,
    pattern: &[Vec<u8>],
    pin_front: bool,
    pin_back: bool,
) -> bool {
    if pin_front && at != 0 {
        return false;
    }
    if pin_back && at + pattern.len() != working.len() {
        return false;
    }
    if at + pattern.len() > working.len() {
        return false;
    }
    for (offset, expected) in pattern.iter().enumerate() {
        let row = &working.rows[at + offset];
        if row.consumed {
            return false;
        }
        if row.raw.as_ref() != expected.as_slice() {
            return false;
        }
    }
    true
}

fn splice_edit(working: &mut WorkingBlob, at: usize, edit: &LineEdit) {
    let remove = edit
        .match_rows
        .len()
        .min(working.rows.len().saturating_sub(at));
    let new_rows = edit.replace_rows.iter().map(|raw| LineRecord {
        raw: Cow::Owned(raw.clone()),
        consumed: true,
    });
    working.rows.splice(at..at + remove, new_rows);
}
