//! Git-compatible `-L` line range parsing and resolution (blame, future log `-L`).
//!
//! Follows upstream `line-range.c` / `parse_range_arg` semantics: numeric ranges,
//! `/regex/` anchors, relative `,+N` / `,-N` ends, and `:funcname` spans.

use std::ops::RangeInclusive;

use crate::config::ConfigSet;
use crate::diff::{is_func_line, PosixEre};
use crate::userdiff::FuncnameMatcher;
use thiserror::Error;

/// Failure while parsing or resolving a `-L` line range specification.
#[derive(Debug, Error, PartialEq, Eq)]
#[non_exhaustive]
pub enum LineRangeError {
    /// The spec could not be parsed or had trailing garbage.
    #[error("-L line range `{spec}`: {detail}")]
    InvalidSpec {
        /// Raw `-L` argument.
        spec: String,
        /// Human-readable detail.
        detail: String,
    },
    /// A line number was zero or negative.
    #[error("-L line range `{spec}`: invalid line number {line}")]
    InvalidLineNumber {
        /// Raw `-L` argument.
        spec: String,
        /// Offending number.
        line: i64,
    },
    /// Relative end `,+0` / `,-0` style empty span.
    #[error("-L line range `{spec}`: empty line range")]
    EmptyRange {
        /// Raw `-L` argument.
        spec: String,
    },
    /// POSIX regex compilation failed.
    #[error("-L parameter `{pattern}`: {detail}")]
    InvalidRegex {
        /// Pattern (without slashes).
        pattern: String,
        /// `regcomp` / engine message.
        detail: String,
    },
    /// Regex did not match from the anchor line.
    #[error("-L parameter `{pattern}` starting at line {start_line}: no match")]
    RegexNoMatch {
        /// Pattern (without slashes).
        pattern: String,
        /// 1-based anchor line searched from.
        start_line: usize,
    },
    /// `:funcname` pattern did not match a function header.
    #[error("-L parameter `{pattern}` starting at line {start_line}: no match")]
    FuncnameNoMatch {
        /// Funcname search pattern.
        pattern: String,
        /// 1-based anchor line searched from.
        start_line: usize,
    },
    /// `:funcname` pattern is invalid.
    #[error("-L parameter `{pattern}`: {detail}")]
    FuncnameInvalidPattern {
        /// Funcname search pattern.
        pattern: String,
        /// Detail.
        detail: String,
    },
    /// `:funcname` match ended at EOF (Git rejects blaming through EOF for func ranges).
    #[error("-L parameter `{pattern}` matches at EOF")]
    FuncnameAtEof {
        /// Funcname search pattern.
        pattern: String,
    },
    /// Resolved range lies outside the file.
    #[error("-L line range `{spec}`: file has only {lines} line(s)")]
    OutOfRange {
        /// Raw `-L` argument.
        spec: String,
        /// Line count in the file at the blamed revision.
        lines: usize,
    },
}

/// Split blob text into lines the same way as [`crate::blame`] (final line without `\n` is still a line).
#[must_use]
pub fn split_content_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut out: Vec<&str> = content.split('\n').collect();
    if out.last() == Some(&"") {
        out.pop();
    }
    out
}

/// Resolve one `-L` spec against `content` at `path` using Git funcname rules.
///
/// `anchor` is the 1-based line where relative searches start (always `1` for a single `-L`).
///
/// # Errors
///
/// Returns [`LineRangeError`] when the spec is invalid or cannot be resolved on `content`.
pub fn resolve_line_range(
    spec: &str,
    content: &str,
    path: &str,
    config: &ConfigSet,
    funcname_matcher: Option<&FuncnameMatcher>,
    anchor: usize,
) -> Result<RangeInclusive<usize>, LineRangeError> {
    let spec_owned = spec.to_owned();
    let lines = split_content_lines(content);
    let line_count = lines.len();
    let mut anchor = anchor.max(1);
    if line_count > 0 && anchor > line_count {
        anchor = line_count + 1;
    }

    let (mut begin, mut end) = parse_range_arg(
        spec,
        &spec_owned,
        content,
        &lines,
        line_count,
        anchor,
        path,
        config,
        funcname_matcher,
    )?;

    if begin == 0 {
        begin = 1;
    }
    if end == 0 || end > line_count {
        end = line_count;
    }
    if begin > line_count && line_count > 0 {
        return Err(LineRangeError::OutOfRange {
            spec: spec_owned,
            lines: line_count,
        });
    }
    if line_count == 0 && (begin > 0 || end > 0) {
        return Err(LineRangeError::OutOfRange {
            spec: spec_owned,
            lines: 0,
        });
    }
    if begin > end {
        std::mem::swap(&mut begin, &mut end);
    }
    Ok(begin..=end)
}

#[expect(clippy::too_many_arguments)]
fn parse_range_arg(
    spec: &str,
    spec_owned: &str,
    content: &str,
    line_slices: &[&str],
    line_count: usize,
    anchor: usize,
    path: &str,
    config: &ConfigSet,
    funcname_matcher: Option<&FuncnameMatcher>,
) -> Result<(usize, usize), LineRangeError> {
    let mut arg = spec.trim();
    let mut begin = 0usize;
    let mut end = 0usize;

    if arg.starts_with(':')
        || (arg.starts_with('^') && arg.get(1..).is_some_and(|r| r.starts_with(':')))
    {
        let rest = parse_range_funcname(
            arg,
            spec_owned,
            content,
            line_slices,
            line_count,
            anchor,
            path,
            config,
            funcname_matcher,
            &mut begin,
            &mut end,
        )?;
        if !rest.is_empty() {
            return Err(LineRangeError::InvalidSpec {
                spec: spec_owned.to_owned(),
                detail: format!("unexpected trailing text `{rest}`"),
            });
        }
        return Ok((begin, end));
    }

    let (rest, b) = parse_loc(arg, spec_owned, content, line_count, -(anchor as i64))?;
    begin = b;
    arg = rest.trim_start();
    if arg.starts_with(',') {
        let (rest, e) = parse_loc(
            arg[1..].trim_start(),
            spec_owned,
            content,
            line_count,
            begin.saturating_add(1) as i64,
        )?;
        end = e;
        arg = rest;
    }
    if !arg.is_empty() {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: format!("unexpected trailing text `{arg}`"),
        });
    }
    Ok((begin, end))
}

fn parse_loc<'a>(
    spec: &'a str,
    spec_owned: &str,
    content: &str,
    _line_count: usize,
    begin_anchor: i64,
) -> Result<(&'a str, usize), LineRangeError> {
    let mut spec = spec.trim();
    if spec.is_empty() {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: "empty range component".into(),
        });
    }

    let mut begin = begin_anchor;

    if begin >= 1 && (spec.starts_with('+') || spec.starts_with('-')) {
        let (num, rest) = parse_signed_number(&spec[1..])?;
        if rest.len() == spec.len().saturating_sub(1) && rest == &spec[1..] {
            return Ok((spec, 0));
        }
        if num == 0 {
            return Err(LineRangeError::EmptyRange {
                spec: spec_owned.to_owned(),
            });
        }
        let mut num = num;
        if spec.starts_with('-') {
            num = -num;
        }
        let resolved = if num > 0 {
            (begin as usize)
                .saturating_add(num as usize)
                .saturating_sub(2)
        } else if num == 0 {
            begin as usize
        } else {
            let shifted = begin + num;
            if shifted > 0 {
                shifted as usize
            } else {
                1
            }
        };
        return Ok((rest, resolved));
    }

    if let Some((num, rest)) = parse_optional_number(spec) {
        if num <= 0 {
            return Err(LineRangeError::InvalidLineNumber {
                spec: spec_owned.to_owned(),
                line: num,
            });
        }
        return Ok((rest, num as usize));
    }

    let mut search_from_bof = false;
    if begin < 0 {
        if !spec.starts_with('^') {
            begin = -begin;
        } else {
            begin = 1;
            search_from_bof = true;
            spec = spec.trim_start_matches('^').trim_start();
        }
    }

    if !spec.starts_with('/') {
        return Ok((spec, 0));
    }

    let (pattern, rest) = parse_slash_regex(spec).ok_or_else(|| LineRangeError::InvalidSpec {
        spec: spec_owned.to_owned(),
        detail: "expected /regex/".into(),
    })?;

    let mut line_index = begin.saturating_sub(1) as usize;
    if search_from_bof {
        line_index = 0;
    }
    let start_byte = line_start_byte(content, line_index);
    let haystack = &content.as_bytes()[start_byte..];
    let re = PosixEre::compile(&pattern, false).map_err(|detail| LineRangeError::InvalidRegex {
        pattern: pattern.clone(),
        detail,
    })?;
    let Some((rel_start, _)) = re.find_from(haystack, 0) else {
        return Err(LineRangeError::RegexNoMatch {
            pattern,
            start_line: line_index.saturating_add(1),
        });
    };
    let match_byte = start_byte + rel_start;
    let line_1based = line_number_for_byte(content, match_byte);
    Ok((rest, line_1based))
}

#[expect(clippy::too_many_arguments)]
fn parse_range_funcname<'a>(
    mut arg: &'a str,
    spec_owned: &str,
    content: &str,
    line_slices: &[&str],
    line_count: usize,
    mut anchor: usize,
    _path: &str,
    _config: &ConfigSet,
    funcname_matcher: Option<&FuncnameMatcher>,
    begin: &mut usize,
    end: &mut usize,
) -> Result<&'a str, LineRangeError> {
    if arg.starts_with('^') {
        anchor = 1;
        arg = &arg[1..];
    }
    if !arg.starts_with(':') {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: "expected :funcname".into(),
        });
    }
    let pattern = parse_funcname_pattern(&arg[1..]).ok_or_else(|| LineRangeError::InvalidSpec {
        spec: spec_owned.to_owned(),
        detail: "expected :funcname".into(),
    })?;
    let (pattern, rest) = pattern;

    let matcher = funcname_matcher;

    anchor = anchor.saturating_sub(1);
    let start_byte = line_start_byte(content, anchor.min(line_count.saturating_sub(1)));
    let re = PosixEre::compile(&pattern, false).map_err(|detail| {
        LineRangeError::FuncnameInvalidPattern {
            pattern: pattern.clone(),
            detail,
        }
    })?;

    let Some(func_start_byte) = find_funcname_matching_regexp(content, start_byte, &re, matcher)
    else {
        return Err(LineRangeError::FuncnameNoMatch {
            pattern,
            start_line: anchor.saturating_add(1),
        });
    };

    let mut begin_idx = 0usize;
    while func_start_byte > line_start_byte(content, begin_idx) && begin_idx + 1 < line_count {
        begin_idx += 1;
    }
    if begin_idx >= line_count {
        return Err(LineRangeError::FuncnameAtEof { pattern });
    }

    let mut end_idx = begin_idx + 1;
    while end_idx < line_count {
        if is_func_line(line_slices[end_idx], matcher) {
            break;
        }
        end_idx += 1;
    }

    *begin = begin_idx + 1;
    *end = end_idx;
    Ok(rest)
}

fn find_funcname_matching_regexp(
    content: &str,
    start_byte: usize,
    re: &PosixEre,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let bytes = content.as_bytes();
    let mut pos = start_byte;
    while pos <= bytes.len() {
        let haystack = &bytes[pos..];
        let (rel_start, rel_end) = re.find_from(haystack, 0)?;
        let match_start = pos + rel_start;
        let match_end = pos + rel_end.max(rel_start + 1);
        let bol = line_start_byte(content, line_number_for_byte(content, match_start) - 1);
        let eol = if match_end >= bytes.len() {
            bytes.len()
        } else {
            bytes[..match_end.min(bytes.len())]
                .iter()
                .rposition(|&b| b == b'\n')
                .map(|n| n + 1)
                .unwrap_or(bol)
                .max(match_end)
        };
        let line_text = std::str::from_utf8(&bytes[bol..eol.min(bytes.len())]).unwrap_or("");
        if is_func_line(line_text, matcher) {
            return Some(bol);
        }
        pos = if eol > pos { eol } else { pos + 1 };
    }
    None
}

fn parse_funcname_pattern(arg: &str) -> Option<(String, &str)> {
    let mut term = 0usize;
    let bytes = arg.as_bytes();
    while term < bytes.len() && bytes[term] != b':' {
        if bytes[term] == b'\\' && term + 1 < bytes.len() {
            term += 2;
        } else {
            term += 1;
        }
    }
    if term == 0 {
        return None;
    }
    let raw = &arg[..term];
    let pattern = unescape_funcname_pattern(raw);
    Some((pattern, &arg[term..]))
}

fn unescape_funcname_pattern(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(next) = chars.next() {
                out.push(next);
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn parse_slash_regex(spec: &str) -> Option<(String, &str)> {
    if !spec.starts_with('/') {
        return None;
    }
    let mut term = 1usize;
    let bytes = spec.as_bytes();
    while term < bytes.len() && bytes[term] != b'/' {
        if bytes[term] == b'\\' && term + 1 < bytes.len() {
            term += 2;
        } else {
            term += 1;
        }
    }
    if term >= bytes.len() {
        return None;
    }
    let pattern = unescape_funcname_pattern(&spec[1..term]);
    Some((pattern, &spec[term + 1..]))
}

fn parse_optional_number(spec: &str) -> Option<(i64, &str)> {
    let spec = spec.trim_start();
    let mut end = 0usize;
    for (i, c) in spec.char_indices() {
        if c.is_ascii_digit() {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let num: i64 = spec[..end].parse().ok()?;
    Some((num, &spec[end..]))
}

fn parse_signed_number(spec: &str) -> Result<(i64, &str), LineRangeError> {
    let Some((num, rest)) = parse_optional_number(spec) else {
        return Ok((0, spec));
    };
    Ok((num, rest))
}

fn line_start_byte(content: &str, line_index: usize) -> usize {
    if line_index == 0 {
        return 0;
    }
    let mut line = 0usize;
    for (idx, b) in content.bytes().enumerate() {
        if b == b'\n' {
            line += 1;
            if line == line_index {
                return idx + 1;
            }
        }
    }
    content.len()
}

fn line_number_for_byte(content: &str, byte: usize) -> usize {
    content[..byte.min(content.len())]
        .bytes()
        .filter(|&b| b == b'\n')
        .count()
        + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_numeric_comma_range() {
        let content = "a\nb\nc\n";
        let r =
            resolve_line_range("2,3", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 2..=3);
    }

    #[test]
    fn resolve_open_end_from_start_line() {
        let content = "a\nb\nc\n";
        let r = resolve_line_range("2", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 2..=3);
    }

    #[test]
    fn resolve_relative_plus() {
        let content = "a\nb\nc\nd\n";
        let r =
            resolve_line_range("2,+2", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 2..=3);
    }

    #[test]
    fn resolve_relative_minus() {
        let content = "a\nb\nc\nd\n";
        let r =
            resolve_line_range("4,-2", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 3..=4);
    }

    #[test]
    fn regex_searches_from_anchor_through_eof() {
        let content = "aaa\nbbb\nccc\n";
        let r =
            resolve_line_range("/bbb/", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 2..=3);
    }

    #[test]
    fn regex_no_match_is_error() {
        let err = resolve_line_range("/zzz/", "aaa\n", "f.txt", &ConfigSet::default(), None, 1)
            .unwrap_err();
        assert!(matches!(err, LineRangeError::RegexNoMatch { .. }));
    }

    #[test]
    fn empty_relative_range_is_error() {
        let err =
            resolve_line_range("1,+0", "a\n", "f.txt", &ConfigSet::default(), None, 1).unwrap_err();
        assert!(matches!(err, LineRangeError::EmptyRange { .. }));
    }

    #[test]
    fn invalid_line_number_zero() {
        let err =
            resolve_line_range("0,1", "a\n", "f.txt", &ConfigSet::default(), None, 1).unwrap_err();
        assert!(matches!(err, LineRangeError::InvalidLineNumber { .. }));
    }

    #[test]
    fn out_of_range_when_start_past_eof() {
        let err = resolve_line_range("5,5", "a\nb\n", "f.txt", &ConfigSet::default(), None, 1)
            .unwrap_err();
        assert!(matches!(err, LineRangeError::OutOfRange { .. }));
    }
}
