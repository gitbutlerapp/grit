//! Line-level history (`git log -L`) — range tracking across diffs.
//!
//! Mirrors Git `line-log.c` behaviour needed for `t4211-line-log`: parse `-L`
//! ranges at the walk tip, map ranges across Myers line diffs toward parents,
//! filter the revision list, and emit synthetic unified hunks.

use crate::config::ConfigSet;
use crate::crlf::{get_file_attrs, load_gitattributes, DiffAttr};
use crate::diff::{detect_renames, diff_trees, zero_oid, DiffEntry};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, parse_tree, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::userdiff::{matcher_for_driver, FuncnameMatcher};
use regex::bytes::RegexBuilder as BytesRegexBuilder;
use similar::{Algorithm, ChangeTag, TextDiff};
use std::collections::{HashMap, HashSet};
use std::path::Path;

/// Half-open line range using 0-based indices (Git internal).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Range {
    pub start: i64,
    pub end: i64,
}

/// Sorted, disjoint, non-empty ranges.
#[derive(Clone, Debug, Default)]
pub struct RangeSet {
    pub ranges: Vec<Range>,
}

impl RangeSet {
    fn append(&mut self, start: i64, end: i64) {
        if start < end {
            self.ranges.push(Range { start, end });
        }
    }

    fn sort_and_merge(&mut self) {
        if self.ranges.is_empty() {
            return;
        }
        self.ranges.sort_by_key(|r| (r.start, r.end));
        let mut out: Vec<Range> = Vec::new();
        for r in std::mem::take(&mut self.ranges) {
            if r.start >= r.end {
                continue;
            }
            if let Some(last) = out.last_mut() {
                if r.start <= last.end {
                    last.end = last.end.max(r.end);
                } else {
                    out.push(r);
                }
            } else {
                out.push(r);
            }
        }
        self.ranges = out;
    }

    fn is_empty(&self) -> bool {
        self.ranges.is_empty()
    }
}

/// One aligned parent/target hunk from a line diff (half-open line indices).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DiffHunk {
    pub parent: Range,
    pub target: Range,
}

/// Sequence of hunks from `collect_diff_ranges` (parent[i] aligns with target[i]).
#[derive(Clone, Debug, Default)]
pub struct DiffRanges {
    pub hunks: Vec<DiffHunk>,
}

impl DiffRanges {
    fn parent_ranges(&self) -> RangeSet {
        let mut s = RangeSet::default();
        for h in &self.hunks {
            s.append(h.parent.start, h.parent.end);
        }
        s.sort_and_merge();
        s
    }

    fn target_ranges(&self) -> RangeSet {
        let mut s = RangeSet::default();
        for h in &self.hunks {
            s.append(h.target.start, h.target.end);
        }
        s.sort_and_merge();
        s
    }
}

fn push_diff_hunk(out: &mut DiffRanges, p0: i64, p1: i64, t0: i64, t1: i64) {
    out.hunks.push(DiffHunk {
        parent: Range { start: p0, end: p1 },
        target: Range { start: t0, end: t1 },
    });
}

fn collect_diff_ranges(old: &str, new: &str) -> DiffRanges {
    let diff = TextDiff::configure()
        .algorithm(Algorithm::Myers)
        .diff_lines(old, new);
    let mut out = DiffRanges::default();
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut o = 0usize;
    let mut n = 0usize;
    let mut in_hunk = false;
    let mut h_o_start = 0usize;
    let mut h_n_start = 0usize;
    let mut h_o_end = 0usize;
    let mut h_n_end = 0usize;

    for change in diff.iter_all_changes() {
        let len = change.value().lines().count().max(1);
        match change.tag() {
            ChangeTag::Equal => {
                if in_hunk {
                    push_diff_hunk(
                        &mut out,
                        h_o_start as i64,
                        h_o_end as i64,
                        h_n_start as i64,
                        h_n_end as i64,
                    );
                    in_hunk = false;
                }
                o = (o + len).min(old_lines.len());
                n = (n + len).min(new_lines.len());
            }
            ChangeTag::Delete => {
                if !in_hunk {
                    in_hunk = true;
                    h_o_start = o;
                    h_n_start = n;
                }
                h_o_end = (o + len).min(old_lines.len());
                h_n_end = n;
                o = h_o_end;
            }
            ChangeTag::Insert => {
                if !in_hunk {
                    in_hunk = true;
                    h_o_start = o;
                    h_n_start = n;
                }
                h_o_end = o;
                h_n_end = (n + len).min(new_lines.len());
                n = h_n_end;
            }
        }
    }
    if in_hunk {
        push_diff_hunk(
            &mut out,
            h_o_start as i64,
            h_o_end as i64,
            h_n_start as i64,
            h_n_end as i64,
        );
    }
    out
}

fn ranges_overlap(a: Range, b: Range) -> bool {
    !(a.end <= b.start || b.end <= a.start)
}

fn diff_ranges_filter_touched(diff: &DiffRanges, rs: &RangeSet) -> DiffRanges {
    let mut out = DiffRanges::default();
    let mut j = 0usize;
    for h in &diff.hunks {
        let tr = h.target;
        while j < rs.ranges.len() && tr.start >= rs.ranges[j].end {
            j += 1;
        }
        if j == rs.ranges.len() {
            break;
        }
        if ranges_overlap(tr, rs.ranges[j]) {
            out.hunks.push(*h);
        }
    }
    out
}

fn range_set_difference(out: &mut RangeSet, a: &RangeSet, b: &RangeSet) {
    let mut j = 0usize;
    for r in &a.ranges {
        let mut start = r.start;
        let end = r.end;
        while start < end {
            while j < b.ranges.len() && start >= b.ranges[j].end {
                j += 1;
            }
            if j >= b.ranges.len() || end <= b.ranges[j].start {
                out.append(start, end);
                break;
            }
            if start >= b.ranges[j].start {
                start = b.ranges[j].end;
            } else if end > b.ranges[j].start {
                if start < b.ranges[j].start {
                    out.append(start, b.ranges[j].start);
                }
                start = b.ranges[j].end;
            }
        }
    }
}

fn range_set_shift_diff(out: &mut RangeSet, rs: &RangeSet, diff: &DiffRanges) {
    let mut j = 0usize;
    let mut offset: i64 = 0;
    for r in &rs.ranges {
        while j < diff.hunks.len() && r.start >= diff.hunks[j].target.start {
            let h = diff.hunks[j];
            offset += (h.parent.end - h.parent.start) - (h.target.end - h.target.start);
            j += 1;
        }
        out.append(r.start + offset, r.end + offset);
    }
}

fn range_set_union(out: &mut RangeSet, a: &RangeSet, b: &RangeSet) {
    let mut i = 0usize;
    let mut j = 0usize;
    while i < a.ranges.len() || j < b.ranges.len() {
        let (start, end, from_a) = match (i < a.ranges.len(), j < b.ranges.len()) {
            (true, true) => {
                let ra = a.ranges[i];
                let rb = b.ranges[j];
                if ra.start < rb.start {
                    (ra.start, ra.end, true)
                } else if rb.start < ra.start {
                    (rb.start, rb.end, false)
                } else if ra.end < rb.end {
                    (ra.start, ra.end, true)
                } else {
                    (rb.start, rb.end, false)
                }
            }
            (true, false) => {
                let ra = a.ranges[i];
                (ra.start, ra.end, true)
            }
            (false, true) => {
                let rb = b.ranges[j];
                (rb.start, rb.end, false)
            }
            (false, false) => break,
        };
        if from_a {
            i += 1;
        } else {
            j += 1;
        }
        if start >= end {
            continue;
        }
        if let Some(last) = out.ranges.last_mut() {
            if last.end < start {
                out.ranges.push(Range { start, end });
            } else if last.end < end {
                last.end = end;
            }
        } else {
            out.ranges.push(Range { start, end });
        }
    }
}

fn range_set_map_across_diff(
    out: &mut RangeSet,
    rs: &RangeSet,
    diff: &DiffRanges,
    touched_out: &mut DiffRanges,
) {
    let touched = diff_ranges_filter_touched(diff, rs);
    let touched_target = touched.target_ranges();
    let touched_parent = touched.parent_ranges();
    let mut tmp1 = RangeSet::default();
    range_set_difference(&mut tmp1, rs, &touched_target);
    let mut tmp2 = RangeSet::default();
    range_set_shift_diff(&mut tmp2, &tmp1, diff);
    range_set_union(out, &tmp2, &touched_parent);
    *touched_out = touched;
}

/// One tracked file with 0-based half-open line ranges.
#[derive(Clone, Debug)]
pub struct LineLogFile {
    pub path: String,
    pub ranges: RangeSet,
}

/// Maps commit OID → active line ranges per path (sorted by path).
pub type LineLogState = HashMap<ObjectId, Vec<LineLogFile>>;

fn search_insert_path(files: &[LineLogFile], path: &str) -> usize {
    match files.binary_search_by_key(&path, |f| f.path.as_str()) {
        Ok(i) => i,
        Err(i) => i,
    }
}

fn line_log_insert(files: &mut Vec<LineLogFile>, path: String, begin: i64, end: i64) {
    let idx = search_insert_path(files, &path);
    if idx < files.len() && files[idx].path == path {
        files[idx].ranges.append(begin, end);
        files[idx].ranges.sort_and_merge();
    } else {
        let mut rs = RangeSet::default();
        rs.append(begin, end);
        files.insert(idx, LineLogFile { path, ranges: rs });
    }
}

fn fill_line_ends(data: &[u8]) -> (i64, Vec<usize>) {
    let mut ends: Vec<usize> = vec![0];
    let mut num = 0usize;
    while num < data.len() {
        if data[num] == b'\n' || num == data.len() - 1 {
            ends.push(num);
        }
        num += 1;
    }
    let lines = (ends.len() as i64) - 1;
    (lines, ends)
}

/// Byte offset where `line` begins (0-based line index). `ends` from [`fill_line_ends`].
fn line_byte_offset(line: i64, ends: &[usize]) -> usize {
    if line <= 0 {
        0
    } else {
        ends[line as usize] + 1
    }
}

fn nth_line<'a>(data: &'a [u8], line: i64, ends: &[usize]) -> &'a [u8] {
    let idx = line_byte_offset(line, ends);
    &data[idx..]
}

/// Line content as Git `nth_line` + `nth_line(line+1)` (used for funcname boundary checks).
fn line_content_git_style<'a>(
    data: &'a [u8],
    line_idx: i64,
    ends: &[usize],
    total_lines: i64,
) -> &'a [u8] {
    let start = line_byte_offset(line_idx, ends);
    let end = if line_idx + 1 >= total_lines {
        data.len()
    } else {
        line_byte_offset(line_idx + 1, ends)
    };
    &data[start..end]
}

/// Parse one `-L` location (Git `parse_loc`). `anchor_for_regex` is `-N` for the first
/// component (`N` = 1-based anchor line) or `begin_A + 1` for the second component.
fn parse_loc<'a>(
    spec: &'a str,
    data: &[u8],
    ends: &[usize],
    lines: i64,
    anchor_for_regex: i64,
    rel_begin_line: i64,
) -> Result<(i64, &'a str)> {
    // Git: `if (1 <= begin && (spec[0] == '+' || spec[0] == '-'))` — the second `-L` component
    // passes `begin = *begin + 1`, which can be `lines + 1` when the first line is the last line.
    let rel_ok = rel_begin_line >= 1;

    let bytes = spec.as_bytes();
    if rel_ok && !spec.is_empty() && (bytes[0] == b'+' || bytes[0] == b'-') {
        let neg = bytes[0] == b'-';
        let rest = &spec[1..];
        let (digits, after) = split_prefix_digits(rest);
        if digits.is_empty() {
            return Err(Error::CorruptObject("-L invalid relative range".to_owned()));
        }
        let num: i64 = digits
            .parse()
            .map_err(|_| Error::CorruptObject("-L invalid relative range".to_owned()))?;
        let num = if neg { -num } else { num };
        let ret = if num > 0 {
            rel_begin_line + num - 2
        } else if num == 0 {
            rel_begin_line
        } else {
            (rel_begin_line + num).max(1)
        };
        return Ok((ret, after));
    }

    let (digits, after_num) = split_prefix_digits(spec);
    if !digits.is_empty() {
        let num: i64 = digits
            .parse()
            .map_err(|_| Error::CorruptObject("-L invalid line number".to_owned()))?;
        if num <= 0 {
            return Err(Error::CorruptObject("-L invalid line number".to_owned()));
        }
        return Ok((num, after_num));
    }

    let mut s = spec;
    let mut anchor_line = anchor_for_regex;
    if anchor_line < 0 {
        anchor_line = -anchor_line;
    } else if s.starts_with('^') {
        anchor_line = 1;
        s = &s[1..];
    }

    let s_bytes = s.as_bytes();
    if s_bytes.first() != Some(&b'/') {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    let mut i = 1usize;
    let mut pattern = String::new();
    while i < s_bytes.len() {
        if s_bytes[i] == b'/' {
            break;
        }
        if s_bytes[i] == b'\\' && i + 1 < s_bytes.len() {
            pattern.push(s_bytes[i + 1] as char);
            i += 2;
            continue;
        }
        pattern.push(s_bytes[i] as char);
        i += 1;
    }
    if i >= s_bytes.len() || s_bytes[i] != b'/' {
        return Err(Error::CorruptObject("malformed -L regex".to_owned()));
    }
    let rest = &s[i + 1..];

    let begin0 = (anchor_line - 1).max(0).min(lines.saturating_sub(1).max(0));
    let line_start = nth_line(data, begin0, ends);
    let re = BytesRegexBuilder::new(&pattern)
        .multi_line(true)
        .build()
        .map_err(|e| Error::CorruptObject(format!("regex: {e}")))?;
    let mat = re
        .find(line_start)
        .ok_or_else(|| Error::CorruptObject("no match".to_owned()))?;
    let mut line_idx = begin0;
    let mut line_beg = line_byte_offset(line_idx, ends);
    let cp = line_byte_offset(begin0, ends) + mat.start();
    while line_idx < lines {
        let next_beg = line_byte_offset(line_idx + 1, ends);
        if line_beg <= cp && cp < next_beg {
            break;
        }
        line_idx += 1;
        line_beg = next_beg;
    }
    Ok((line_idx + 1, rest))
}

fn split_prefix_digits(s: &str) -> (&str, &str) {
    let end = s
        .as_bytes()
        .iter()
        .take_while(|b| b.is_ascii_digit())
        .count();
    (&s[..end], &s[end..])
}

fn match_funcname_line(line: &[u8]) -> bool {
    let Some(&c) = line.first() else {
        return false;
    };
    c.is_ascii_alphabetic() || c == b'_' || c == b'$'
}

fn line_matches_funcname(matcher: Option<&FuncnameMatcher>, line: &[u8]) -> bool {
    let mut end = line.len();
    while end > 0 && (line[end - 1] == b'\n' || line[end - 1] == b'\r') {
        end -= 1;
    }
    let body = &line[..end];
    if let Some(m) = matcher {
        let s = String::from_utf8_lossy(body);
        m.match_line(s.as_ref()).is_some()
    } else {
        match_funcname_line(body)
    }
}

/// Git only applies `userdiff` funcname rules when `diff=<driver>` is set in `.gitattributes`
/// (`userdiff_find_by_path`); filename-based driver guessing is not used for `-L :pat:file`.
fn funcname_matcher_for_path(
    git_dir: &Path,
    work_tree: Option<&Path>,
    path: &str,
) -> Option<FuncnameMatcher> {
    let wt = work_tree?;
    let rules = load_gitattributes(wt);
    let config = ConfigSet::load(Some(git_dir), true).unwrap_or_default();
    let fa = get_file_attrs(&rules, path, false, &config);
    let DiffAttr::Driver(ref name) = fa.diff_attr else {
        return None;
    };
    matcher_for_driver(&config, name).ok().flatten()
}

fn parse_range_funcname<'a>(
    arg: &'a str,
    data: &[u8],
    ends: &[usize],
    lines: i64,
    mut anchor: i64,
    userdiff: Option<&FuncnameMatcher>,
) -> Result<(i64, i64, &'a str)> {
    let mut s = arg;
    if s.starts_with('^') && s.get(1..2) == Some(":") {
        anchor = 1;
        s = &s[2..];
    } else if s.starts_with(':') {
        s = &s[1..];
    } else {
        return Err(Error::CorruptObject("bad funcname range".to_owned()));
    }

    let mut i = 0usize;
    let b = s.as_bytes();
    while i < b.len() && b[i] != b':' {
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 2;
        } else {
            i += 1;
        }
    }
    if i >= b.len() {
        return Err(Error::CorruptObject("bad funcname range".to_owned()));
    }
    let pattern = &s[..i];
    let after_colon = i + 1;

    let anchor0 = (anchor - 1).max(0).min(lines.saturating_sub(1).max(0));

    // Git treats `^`, `$`, and `.*` in funcname mode as "any function-like line" (t4211).
    if pattern == "^" || pattern == "$" || pattern == ".*" {
        let mut line_idx = anchor0;
        let mut found_begin = None;
        while line_idx < lines {
            let slice = line_content_git_style(data, line_idx, ends, lines);
            if line_matches_funcname(userdiff, slice) {
                found_begin = Some(line_idx);
                break;
            }
            line_idx += 1;
        }
        let begin = found_begin.ok_or_else(|| Error::CorruptObject("no match".to_owned()))?;
        let mut end = begin + 1;
        while end < lines {
            let slice = line_content_git_style(data, end, ends, lines);
            if line_matches_funcname(userdiff, slice) {
                break;
            }
            end += 1;
        }
        let tail = s.get(after_colon..).unwrap_or("");
        return Ok((begin + 1, end, tail));
    }

    let start_search = nth_line(data, anchor0, ends);
    let re = BytesRegexBuilder::new(pattern)
        .multi_line(true)
        .build()
        .map_err(|e| Error::CorruptObject(format!("regex: {e}")))?;

    let mut p = start_search;
    let found_bol = loop {
        let Some(m) = re.find(p) else {
            break None;
        };
        let match_start = p.as_ptr() as usize - data.as_ptr() as usize + m.start();
        let mut bol = match_start;
        while bol > 0 && data[bol - 1] != b'\n' {
            bol -= 1;
        }
        let mut eol = match_start;
        while eol < data.len() && data[eol] != b'\n' {
            eol += 1;
        }
        if eol < data.len() {
            eol += 1;
        }
        let line_slice = &data[bol..eol.min(data.len())];
        if line_matches_funcname(userdiff, line_slice) {
            break Some(bol);
        }
        p = &data[eol.min(data.len())..];
        if p.is_empty() {
            break None;
        }
    };

    let bol = found_bol.ok_or_else(|| Error::CorruptObject("no match".to_owned()))?;

    let mut begin = 0i64;
    while begin < lines {
        if line_byte_offset(begin, ends) == bol {
            break;
        }
        begin += 1;
    }
    if begin >= lines {
        return Err(Error::CorruptObject("funcname matches at EOF".to_owned()));
    }

    let mut end = begin + 1;
    while end < lines {
        let slice = line_content_git_style(data, end, ends, lines);
        if line_matches_funcname(userdiff, slice) {
            break;
        }
        end += 1;
    }

    let tail = s.get(after_colon..).unwrap_or("");
    Ok((begin + 1, end, tail))
}

/// Split `/re/` or `/re/,/re2/` into one or two slash-delimited regex specs (no path).
fn split_slash_regex_range(arg: &str) -> Result<(&str, Option<&str>)> {
    let b = arg.as_bytes();
    if b.first() != Some(&b'/') {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    let mut pos = 0usize;
    let start_first = pos;
    pos += 1;
    while pos < b.len() {
        if b[pos] == b'\\' {
            pos = (pos + 2).min(b.len());
            continue;
        }
        if b[pos] == b'/' {
            pos += 1;
            break;
        }
        pos += 1;
    }
    if pos > b.len() || pos == start_first + 1 {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    let first = &arg[start_first..pos];
    if pos >= arg.len() {
        return Ok((first, None));
    }
    if b.get(pos).copied() != Some(b',') {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    pos += 1;
    if b.get(pos).copied() != Some(b'/') {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    let start_second = pos;
    pos += 1;
    while pos < b.len() {
        if b[pos] == b'\\' {
            pos = (pos + 2).min(b.len());
            continue;
        }
        if b[pos] == b'/' {
            pos += 1;
            break;
        }
        pos += 1;
    }
    if pos != arg.len() {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    Ok((first, Some(&arg[start_second..pos])))
}

fn parse_range_arg(
    arg: &str,
    data: &[u8],
    ends: &[usize],
    lines: i64,
    anchor: i64,
    userdiff: Option<&FuncnameMatcher>,
) -> Result<(i64, i64)> {
    let (begin, end) = if arg.starts_with(':') || arg.starts_with("^:") {
        let (b, e, rest) = parse_range_funcname(arg, data, ends, lines, anchor, userdiff)?;
        if !rest.is_empty() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        (b, e)
    } else if arg.starts_with('/') {
        let (a, second) = split_slash_regex_range(arg)?;
        let (b1, r1) = parse_loc(a, data, ends, lines, -anchor, 0)?;
        if !r1.is_empty() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        let (b2, r2) = if let Some(s2) = second {
            parse_loc(s2, data, ends, lines, b1 + 1, b1 + 1)?
        } else {
            (b1, "")
        };
        if !r2.is_empty() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        let mut b1 = b1;
        let mut b2 = b2;
        if b1 != 0 && b2 != 0 && b2 < b1 {
            std::mem::swap(&mut b1, &mut b2);
        }
        (b1, b2)
    } else if arg.contains(',') {
        let comma = arg
            .find(',')
            .ok_or_else(|| Error::CorruptObject("malformed -L argument".to_owned()))?;
        let (a, b) = arg.split_at(comma);
        let b = &b[1..];
        let (b1, r1) = if a.is_empty() {
            (1i64, "")
        } else {
            parse_loc(a, data, ends, lines, -anchor, 0)?
        };
        if !r1.is_empty() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        let (b2, r2) = if b.is_empty() {
            (b1, "")
        } else {
            parse_loc(b, data, ends, lines, b1 + 1, b1 + 1)?
        };
        if !r2.is_empty() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        let mut b1 = b1;
        let mut b2 = b2;
        if b1 != 0 && b2 != 0 && b2 < b1 {
            std::mem::swap(&mut b1, &mut b2);
        }
        (b1, b2)
    } else {
        let (n, rest) = parse_loc(arg, data, ends, lines, -anchor, 0)?;
        if !rest.is_empty() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        // Git leaves `end` at 0 when the comma branch is absent; `parse_lines` then sets
        // `end = lines` ("from N through end of file").
        (n, 0)
    };

    if (lines == 0 && (begin != 0 || end != 0)) || (lines > 0 && lines < begin) {
        return Err(Error::CorruptObject(format!("file has only {lines} lines")));
    }
    let mut begin = begin;
    let mut end = end;
    if begin < 1 {
        begin = 1;
    }
    if end < 1 || lines < end {
        end = lines;
    }
    begin -= 1;
    Ok((begin, end))
}

/// Find index of `:` that ends a `:pattern` segment (backslash-escaped colons skipped).
fn scan_funcname_pattern_colon(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 0usize;
    while i < b.len() {
        if b[i] == b':' {
            return Some(i);
        }
        if b[i] == b'\\' && i + 1 < b.len() {
            i += 2;
        } else {
            i += 1;
        }
    }
    None
}

/// Parse `/re/` or `/re/,/re2/` prefix; returns `(full_range_prefix, path)` after final `:path`.
fn split_regex_line_range(spec: &str) -> Result<(&str, &str)> {
    let b = spec.as_bytes();
    if b.first() != Some(&b'/') {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    let mut pos = 0usize;
    loop {
        if pos >= b.len() || b[pos] != b'/' {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        pos += 1;
        while pos < b.len() {
            if b[pos] == b'\\' {
                pos = (pos + 2).min(b.len());
                continue;
            }
            if b[pos] == b'/' {
                pos += 1;
                break;
            }
            pos += 1;
        }
        if pos > b.len() {
            return Err(Error::CorruptObject("malformed -L argument".to_owned()));
        }
        if pos < b.len() && b[pos] == b',' {
            pos += 1;
            continue;
        }
        break;
    }
    if pos >= b.len() || b[pos] != b':' {
        return Err(Error::CorruptObject("malformed -L argument".to_owned()));
    }
    let path = &spec[pos + 1..];
    if path.is_empty() {
        return Err(Error::CorruptObject(
            "-L argument not 'start,end:file'".to_owned(),
        ));
    }
    Ok((&spec[..pos], path))
}

/// Split `-L` argument into `(range_spec, path)`.
fn split_l_arg(spec: &str) -> Result<(&str, &str)> {
    if spec.is_empty() {
        return Err(Error::CorruptObject(
            "-L argument not 'start,end:file'".to_owned(),
        ));
    }
    if let Some(rest) = spec.strip_prefix("^:") {
        let idx = scan_funcname_pattern_colon(rest)
            .ok_or_else(|| Error::CorruptObject("-L argument not 'start,end:file'".to_owned()))?;
        let path = &rest[idx + 1..];
        if path.is_empty() {
            return Err(Error::CorruptObject(
                "-L argument not 'start,end:file'".to_owned(),
            ));
        }
        // `^:pattern:path` — include trailing `:` so `parse_range_funcname` sees `pattern:` after stripping `^:`.
        return Ok((&spec[..2 + idx + 1], path));
    }
    if let Some(rest) = spec.strip_prefix(':') {
        let idx = scan_funcname_pattern_colon(rest)
            .ok_or_else(|| Error::CorruptObject("-L argument not 'start,end:file'".to_owned()))?;
        let path = &rest[idx + 1..];
        if path.is_empty() {
            return Err(Error::CorruptObject(
                "-L argument not 'start,end:file'".to_owned(),
            ));
        }
        // `:pattern:path` — include delimiter before path in range spec.
        return Ok((&spec[..1 + idx + 1], path));
    }
    if spec.starts_with('/') {
        return split_regex_line_range(spec)
            .map_err(|_| Error::CorruptObject("-L argument not 'start,end:file'".to_owned()));
    }
    let idx = spec
        .rfind(':')
        .ok_or_else(|| Error::CorruptObject("-L argument not 'start,end:file'".to_owned()))?;
    if idx == 0 {
        return Err(Error::CorruptObject(
            "-L argument not 'start,end:file'".to_owned(),
        ));
    }
    let path = &spec[idx + 1..];
    if path.is_empty() {
        return Err(Error::CorruptObject(
            "-L argument not 'start,end:file'".to_owned(),
        ));
    }
    Ok((&spec[..idx], path))
}

fn read_blob_at_path(odb: &Odb, tree_oid: &ObjectId, path: &str) -> Result<Vec<u8>> {
    let mut current = *tree_oid;
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for (pi, part) in parts.iter().enumerate() {
        let obj = odb.read(&current)?;
        if obj.kind != ObjectKind::Tree {
            return Err(Error::CorruptObject("not a tree".to_owned()));
        }
        let entries = parse_tree(&obj.data)?;
        let name = part.as_bytes();
        let mut found: Option<&TreeEntry> = None;
        for e in &entries {
            if e.name == name {
                found = Some(e);
                break;
            }
        }
        let entry = found
            .ok_or_else(|| Error::CorruptObject(format!("There is no path {path} in commit")))?;
        if pi + 1 == parts.len() {
            if (entry.mode & 0o170000) != 0o100000 {
                return Err(Error::CorruptObject(format!(
                    "There is no path {path} in commit"
                )));
            }
            let blob = odb.read(&entry.oid)?;
            if blob.kind != ObjectKind::Blob {
                return Err(Error::CorruptObject("not a blob".to_owned()));
            }
            return Ok(blob.data);
        }
        if entry.mode != 0o040000 {
            return Err(Error::CorruptObject(format!(
                "There is no path {path} in commit"
            )));
        }
        current = entry.oid;
    }
    Err(Error::CorruptObject(format!(
        "There is no path {path} in commit"
    )))
}

/// Parse all `-L` specs at `tip_oid` into per-file merged ranges.
pub fn parse_line_log_ranges(
    odb: &Odb,
    git_dir: &Path,
    work_tree: Option<&Path>,
    tip_oid: &ObjectId,
    specs: &[String],
) -> Result<Vec<LineLogFile>> {
    let tip_obj = odb.read(tip_oid)?;
    if tip_obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject("tip not a commit".to_owned()));
    }
    let tip_commit = parse_commit(&tip_obj.data)?;
    let tree_oid = tip_commit.tree;

    let mut files: Vec<LineLogFile> = Vec::new();

    for spec in specs {
        let (range_part, path) = split_l_arg(spec)?;
        let userdiff = funcname_matcher_for_path(git_dir, work_tree, path);
        let blob_data = read_blob_at_path(odb, &tree_oid, path)?;
        let (lines, ends) = fill_line_ends(&blob_data);

        let idx = search_insert_path(&files, path);
        let anchor = if idx < files.len() && files[idx].path == path {
            files[idx].ranges.ranges.last().map_or(1, |x| x.end + 1)
        } else if idx > 0 && files[idx - 1].path == path {
            files[idx - 1].ranges.ranges.last().map_or(1, |x| x.end + 1)
        } else {
            1
        };

        let (begin, end) = parse_range_arg(
            range_part,
            &blob_data,
            &ends,
            lines,
            anchor,
            userdiff.as_ref(),
        )?;
        line_log_insert(&mut files, path.to_owned(), begin, end);
    }

    for f in &mut files {
        f.ranges.sort_and_merge();
    }
    Ok(files)
}

fn copy_line_state(files: &[LineLogFile]) -> Vec<LineLogFile> {
    files
        .iter()
        .map(|f| LineLogFile {
            path: f.path.clone(),
            ranges: RangeSet {
                ranges: f.ranges.ranges.clone(),
            },
        })
        .collect()
}

fn filter_entries_for_paths(entries: Vec<DiffEntry>, paths: &[String]) -> Vec<DiffEntry> {
    let set: HashSet<&str> = paths.iter().map(|p| p.as_str()).collect();
    entries
        .into_iter()
        .filter(|e| {
            e.new_path
                .as_deref()
                .map(|p| set.contains(p.to_string_lossy().as_ref()))
                .unwrap_or(false)
        })
        .collect()
}

fn paths_from_range_files(range: &[LineLogFile]) -> Vec<String> {
    let mut v: Vec<String> = range
        .iter()
        .filter(|f| !f.ranges.is_empty())
        .map(|f| f.path.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

fn diff_tree_pair(
    odb: &Odb,
    old_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
    paths: &[String],
    rename_threshold: u32,
) -> Result<Vec<DiffEntry>> {
    let old = old_tree.copied();
    let mut entries = diff_trees(odb, old.as_ref(), Some(new_tree), "")?;
    if rename_threshold < 100 {
        entries = detect_renames(odb, None, entries, rename_threshold);
    }
    Ok(filter_entries_for_paths(entries, paths))
}

struct FileDiffArtifacts {
    old_path: String,
    new_path: String,
    old_text: String,
    new_text: String,
    diff: DiffRanges,
}

/// Captured diff input for `format_line_log_diff` (one file per hunk group).
#[derive(Clone, Debug)]
pub struct LineLogDisplay {
    pub old_path: String,
    pub new_path: String,
    pub old_bytes: Vec<u8>,
    pub new_bytes: Vec<u8>,
    pub commit_ranges: RangeSet,
    pub touched: DiffRanges,
}

fn load_pair_content(
    odb: &Odb,
    entry: &DiffEntry,
    range_files: &[LineLogFile],
) -> Result<Option<FileDiffArtifacts>> {
    let new_path = entry
        .new_path
        .as_deref()
        .ok_or_else(|| Error::CorruptObject("diff entry missing path".to_owned()))?;
    let rg = range_files.iter().find(|f| new_path == f.path.as_str());
    let Some(rg) = rg else {
        return Ok(None);
    };
    if rg.ranges.is_empty() {
        return Ok(None);
    }

    let z = zero_oid();
    let new_bytes = if entry.new_oid == z {
        Vec::new()
    } else {
        odb.read(&entry.new_oid)?.data
    };
    let old_bytes = if entry.old_oid == z {
        Vec::new()
    } else {
        odb.read(&entry.old_oid)?.data
    };
    let old_text = String::from_utf8_lossy(&old_bytes).into_owned();
    let new_text = String::from_utf8_lossy(&new_bytes).into_owned();
    let diff = collect_diff_ranges(&old_text, &new_text);
    let old_path = entry
        .old_path
        .as_deref()
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|| "/dev/null".to_owned());
    Ok(Some(FileDiffArtifacts {
        old_path,
        new_path: new_path.to_string_lossy().into_owned(),
        old_text,
        new_text,
        diff,
    }))
}

fn process_diff_filepair(
    artifacts: &FileDiffArtifacts,
    range_files: &mut [LineLogFile],
) -> Result<Option<DiffRanges>> {
    let idx = range_files
        .iter()
        .position(|f| f.path == artifacts.new_path)
        .ok_or_else(|| Error::CorruptObject("range file missing".to_owned()))?;
    if range_files[idx].ranges.is_empty() {
        return Ok(None);
    }

    let mut out_ranges = RangeSet::default();
    let mut touched = DiffRanges::default();
    range_set_map_across_diff(
        &mut out_ranges,
        &range_files[idx].ranges,
        &artifacts.diff,
        &mut touched,
    );

    range_files[idx].path = artifacts.old_path.clone();
    range_files[idx].ranges = out_ranges;

    if touched.hunks.is_empty() {
        Ok(None)
    } else {
        Ok(Some(touched))
    }
}

fn process_all_files(
    odb: &Odb,
    queue: &[DiffEntry],
    range: &[LineLogFile],
) -> Result<(Vec<LineLogFile>, Vec<LineLogDisplay>, bool)> {
    let mut out = copy_line_state(range);
    let mut displays: Vec<LineLogDisplay> = Vec::new();
    let mut changed = false;
    for entry in queue {
        let Some(art) = load_pair_content(odb, entry, range)? else {
            continue;
        };
        let commit_ranges = range
            .iter()
            .find(|f| f.path == art.new_path)
            .map(|f| RangeSet {
                ranges: f.ranges.ranges.clone(),
            })
            .unwrap_or_default();
        if let Some(touched) = process_diff_filepair(&art, &mut out)? {
            // A hunk that only adds lines still has an empty parent span but is not TREESAME
            // when it overlaps the tracked target range (Git keeps e.g. `change f()`).
            if touched
                .hunks
                .iter()
                .any(|h| (h.parent.start < h.parent.end) || (h.target.start < h.target.end))
            {
                changed = true;
            }
            displays.push(LineLogDisplay {
                old_path: art.old_path.clone(),
                new_path: art.new_path.clone(),
                old_bytes: art.old_text.as_bytes().to_vec(),
                new_bytes: art.new_text.as_bytes().to_vec(),
                commit_ranges,
                touched,
            });
        }
    }
    Ok((out, displays, changed))
}

fn process_ranges_ordinary_commit(
    odb: &Odb,
    parents: &[ObjectId],
    tree_oid: &ObjectId,
    range: &[LineLogFile],
    rename_threshold: u32,
    state: &mut LineLogState,
    display: &mut HashMap<ObjectId, Vec<LineLogDisplay>>,
    commit_oid: ObjectId,
) -> Result<bool> {
    let parent = parents.first();
    let parent_tree = parent
        .map(|p| parse_commit(&odb.read(p)?.data).map(|c| c.tree))
        .transpose()?;
    let path_keys = paths_from_range_files(range);
    let queue = diff_tree_pair(
        odb,
        parent_tree.as_ref(),
        tree_oid,
        &path_keys,
        rename_threshold,
    )?;
    let (parent_range, disp, changed) = process_all_files(odb, &queue, range)?;
    if !disp.is_empty() {
        display.insert(commit_oid, disp);
    }
    if let Some(p) = parent {
        state.insert(*p, parent_range);
    }
    Ok(changed)
}

fn process_ranges_merge_commit(
    odb: &Odb,
    parents: &[ObjectId],
    tree_oid: &ObjectId,
    range: &[LineLogFile],
    rename_threshold: u32,
    first_parent_only: bool,
    state: &mut LineLogState,
    display: &mut HashMap<ObjectId, Vec<LineLogDisplay>>,
    commit_oid: ObjectId,
) -> Result<bool> {
    let nparents = if first_parent_only { 1 } else { parents.len() };

    let mut candidates: Vec<Vec<LineLogFile>> = Vec::with_capacity(nparents);

    for i in 0..nparents {
        let p = parents[i];
        let ptree = parse_commit(&odb.read(&p)?.data)?.tree;
        let path_keys = paths_from_range_files(range);
        let queue = diff_tree_pair(odb, Some(&ptree), tree_oid, &path_keys, rename_threshold)?;
        let (prange, _, changed_here) = process_all_files(odb, &queue, range)?;
        if !changed_here {
            state.insert(p, prange);
            state.remove(&commit_oid);
            return Ok(false);
        }
        candidates.push(prange);
    }

    for (i, p) in parents.iter().enumerate().take(nparents) {
        state.insert(*p, candidates[i].clone());
    }
    state.remove(&commit_oid);

    let p0 = parents[0];
    let ptree = parse_commit(&odb.read(&p0)?.data)?.tree;
    let path_keys = paths_from_range_files(range);
    let queue = diff_tree_pair(odb, Some(&ptree), tree_oid, &path_keys, rename_threshold)?;
    // Display uses the commit-side ranges (`range`), matching Git's merge limitation.
    let (_, disp, _) = process_all_files(odb, &queue, range)?;
    if !disp.is_empty() {
        display.insert(commit_oid, disp);
    }
    Ok(true)
}

/// Filter full history to commits that affect the tracked line ranges; map ranges to parents.
pub fn line_log_filter_commits(
    odb: &Odb,
    commits: Vec<ObjectId>,
    tip: ObjectId,
    initial: Vec<LineLogFile>,
    rename_threshold: u32,
    first_parent_only: bool,
) -> Result<(
    Vec<ObjectId>,
    LineLogState,
    HashMap<ObjectId, Vec<LineLogDisplay>>,
)> {
    let mut state: LineLogState = HashMap::new();
    state.insert(tip, initial);
    let mut display_map: HashMap<ObjectId, Vec<LineLogDisplay>> = HashMap::new();
    let mut keep_commit: HashMap<ObjectId, bool> = HashMap::new();

    for &oid in &commits {
        let Some(range) = state.get(&oid).cloned() else {
            continue;
        };
        if range.iter().all(|f| f.ranges.is_empty()) {
            continue;
        }

        let c = parse_commit(&odb.read(&oid)?.data)?;
        let parents = c.parents.clone();

        let show = if parents.len() > 1 {
            process_ranges_merge_commit(
                odb,
                &parents,
                &c.tree,
                &range,
                rename_threshold,
                first_parent_only,
                &mut state,
                &mut display_map,
                oid,
            )?
        } else {
            process_ranges_ordinary_commit(
                odb,
                &parents,
                &c.tree,
                &range,
                rename_threshold,
                &mut state,
                &mut display_map,
                oid,
            )?
        };
        keep_commit.insert(oid, show);
    }

    let filtered: Vec<ObjectId> = commits
        .iter()
        .copied()
        .filter(|o| *keep_commit.get(o).unwrap_or(&false))
        .collect();

    Ok((filtered, state, display_map))
}

/// First parent after skipping commits not in `filtered` (Git `--parents` / line-log rewrite).
pub fn rewritten_first_parent(
    odb: &Odb,
    oid: &ObjectId,
    filtered: &HashSet<ObjectId>,
) -> Result<Option<ObjectId>> {
    let c = parse_commit(&odb.read(oid)?.data)?;
    let Some(mut p) = c.parents.first().copied() else {
        return Ok(None);
    };
    while !filtered.contains(&p) {
        let pc = parse_commit(&odb.read(&p)?.data)?;
        let Some(np) = pc.parents.first().copied() else {
            return Ok(Some(p));
        };
        p = np;
    }
    Ok(Some(p))
}

/// Format hacky line-log hunk (Git `dump_diff_hacky_one`).
pub fn format_line_log_diff(
    prefix: &str,
    old_path: &str,
    new_path: &str,
    old_data: &[u8],
    new_data: &[u8],
    ranges: &RangeSet,
    touched: &DiffRanges,
) -> String {
    let (p_lines, p_ends) = fill_line_ends(old_data);
    let (t_lines, t_ends) = fill_line_ends(new_data);
    let _ = (p_lines, t_lines);

    let mut out = String::new();
    if touched.hunks.is_empty() {
        return out;
    }

    out.push_str(prefix);
    out.push('\n');
    out.push_str(prefix);
    out.push_str("diff --git a/");
    out.push_str(new_path);
    out.push_str(" b/");
    out.push_str(new_path);
    out.push('\n');
    out.push_str(prefix);
    if old_path == "/dev/null" {
        out.push_str("--- /dev/null\n");
    } else {
        out.push_str("--- a/");
        out.push_str(old_path);
        out.push('\n');
    }
    out.push_str(prefix);
    out.push_str("+++ b/");
    out.push_str(new_path);
    out.push('\n');

    let mut j = 0usize;
    for i in 0..ranges.ranges.len() {
        let t_start = ranges.ranges[i].start;
        let t_end = ranges.ranges[i].end;
        let mut t_cur = t_start;

        if j > 0 && touched.hunks[j - 1].target.end > t_start {
            j -= 1;
        }
        while j < touched.hunks.len() && touched.hunks[j].target.end < t_start {
            j += 1;
        }
        if j >= touched.hunks.len() || touched.hunks[j].target.start >= t_end {
            continue;
        }

        let mut j_last = j;
        while j_last < touched.hunks.len() && touched.hunks[j_last].target.start < t_end {
            j_last += 1;
        }
        if j_last > j {
            j_last -= 1;
        }

        let p_start = if t_start < touched.hunks[j].target.start {
            touched.hunks[j].parent.start - (touched.hunks[j].target.start - t_start)
        } else {
            touched.hunks[j].parent.start
        };
        let p_end = if t_end > touched.hunks[j_last].target.end {
            touched.hunks[j_last].parent.end + (t_end - touched.hunks[j_last].target.end)
        } else {
            touched.hunks[j_last].parent.end
        };

        let (p_start, p_end) = if p_start == 0 && p_end == 0 {
            (-1, -1)
        } else {
            (p_start, p_end)
        };

        out.push_str(prefix);
        out.push_str(&format!(
            "@@ -{},{} +{},{} @@\n",
            p_start + 1,
            p_end - p_start,
            t_start + 1,
            t_end - t_start
        ));

        while j < touched.hunks.len() && touched.hunks[j].target.start < t_end {
            while t_cur < touched.hunks[j].target.start {
                print_line(&mut out, prefix, ' ', t_cur, &t_ends, new_data);
                t_cur += 1;
            }
            let ps = touched.hunks[j].parent.start;
            let pe = touched.hunks[j].parent.end;
            for k in ps..pe {
                print_line(&mut out, prefix, '-', k, &p_ends, old_data);
            }
            while t_cur < touched.hunks[j].target.end && t_cur < t_end {
                print_line(&mut out, prefix, '+', t_cur, &t_ends, new_data);
                t_cur += 1;
            }
            j += 1;
        }
        while t_cur < t_end {
            print_line(&mut out, prefix, ' ', t_cur, &t_ends, new_data);
            t_cur += 1;
        }
    }

    out
}

fn print_line(out: &mut String, prefix: &str, first: char, line: i64, ends: &[usize], data: &[u8]) {
    let begin = if line == 0 {
        0
    } else {
        ends[line as usize] + 1
    };
    let end = if (line + 1) as usize >= ends.len() {
        data.len()
    } else {
        ends[(line + 1) as usize] + 1
    };
    let mut end2 = end.min(data.len());
    let had_nl = if end2 > begin && data[end2 - 1] == b'\n' {
        end2 -= 1;
        true
    } else {
        false
    };
    let slice = &data[begin..end2];
    out.push_str(prefix);
    out.push(first);
    out.push_str(&String::from_utf8_lossy(slice));
    out.push('\n');
    if !had_nl {
        out.push_str(prefix);
        out.push_str("\\ No newline at end of file\n");
    }
}
