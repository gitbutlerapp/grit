//! Resolve Git-style `-L` line ranges for blame (and future log `-L`).
//!
//! Behavior matches [git-scm.com line-range documentation](https://git-scm.com/docs/git-blame#Documentation/git-blame.txt--Lltstartgtltendgt):
//! numeric endpoints, `/search/` (POSIX basic regex), relative `,+N` / `,-N` tails,
//! and `:symbol` spans using the same funcname rules as diff hunk headers.
//!
//! ```rust
//! use grit_lib::line_range::split_content_lines;
//!
//! assert_eq!(split_content_lines("a\nb\n"), ["a", "b"]);
//! ```

use std::ops::RangeInclusive;

use crate::config::ConfigSet;
use crate::diff::{is_func_line, PosixBre};
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

/// Resolve one `-L` spec against `content` using Git funcname rules when `matcher` is set.
///
/// `anchor` is the 1-based line where chained `-L` ranges would continue (always `1` for a single `-L`).
///
/// `path` and `config` are accepted for API stability; resolution uses `matcher` and `content` only.
///
/// # Errors
///
/// Returns [`LineRangeError`] when the spec is invalid or cannot be resolved on `content`.
pub fn resolve_line_range(
    spec: &str,
    content: &str,
    _path: &str,
    _config: &ConfigSet,
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

    let view = TextView::new(content, &lines);
    let draft = decode_range_spec(
        spec.trim(),
        &spec_owned,
        &view,
        line_count,
        anchor,
        funcname_matcher,
    )?;
    finalize_inclusive_range(draft, line_count, &spec_owned)
}

/// Parsed but not yet clamped 1-based endpoints (`0` means “unset”).
struct DraftRange {
    start: usize,
    end: usize,
}

struct TextView<'a> {
    raw: &'a str,
    lines: &'a [&'a str],
}

impl<'a> TextView<'a> {
    fn new(raw: &'a str, lines: &'a [&'a str]) -> Self {
        Self { raw, lines }
    }

    fn line_count(&self) -> usize {
        self.lines.len()
    }

    fn line_byte_range(&self, index_0: usize) -> (usize, usize) {
        let start = byte_offset_of_line(self.raw, index_0);
        let end = if index_0 + 1 < self.line_count() {
            byte_offset_of_line(self.raw, index_0 + 1)
        } else {
            self.raw.len()
        };
        let end = if end > start && self.raw.as_bytes()[end - 1] == b'\n' {
            end - 1
        } else {
            end
        };
        (start, end)
    }

    fn line_text(&self, index_0: usize) -> &str {
        let (start, end) = self.line_byte_range(index_0);
        &self.raw[start..end]
    }
}

fn decode_range_spec(
    spec: &str,
    spec_owned: &str,
    view: &TextView<'_>,
    line_count: usize,
    anchor_1: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Result<DraftRange, LineRangeError> {
    if spec.starts_with(':') || spec.starts_with("^:") {
        return decode_symbol_range(spec, spec_owned, view, line_count, anchor_1, matcher);
    }

    let (start_token, tail) = split_first_comma(spec);
    let start = resolve_endpoint(
        start_token,
        spec_owned,
        view,
        EndpointContext {
            role: EndpointRole::Start,
            anchor_1,
            pivot_1: 0,
        },
    )?;
    let mut draft = DraftRange { start, end: 0 };
    if let Some(end_token) = tail {
        let end = resolve_endpoint(
            end_token.trim_start(),
            spec_owned,
            view,
            EndpointContext {
                role: EndpointRole::End,
                anchor_1,
                pivot_1: draft.start,
            },
        )?;
        draft.end = end;
    }
    Ok(draft)
}

enum EndpointRole {
    Start,
    End,
}

struct EndpointContext {
    role: EndpointRole,
    anchor_1: usize,
    pivot_1: usize,
}

fn resolve_endpoint(
    token: &str,
    spec_owned: &str,
    view: &TextView<'_>,
    ctx: EndpointContext,
) -> Result<usize, LineRangeError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: "empty range component".into(),
        });
    }

    let (token, scan_from_line_one) = if let Some(rest) = token.strip_prefix('^') {
        (rest.trim_start(), true)
    } else {
        (token, false)
    };

    let relative_base = match ctx.role {
        EndpointRole::Start => -(ctx.anchor_1 as i64),
        EndpointRole::End => (ctx.pivot_1.saturating_add(1)) as i64,
    };

    if relative_base >= 1 && (token.starts_with('+') || token.starts_with('-')) {
        return relative_offset_line(token, spec_owned, relative_base);
    }

    if let Some((num, rest)) = leading_decimal(token) {
        if num <= 0 {
            return Err(LineRangeError::InvalidLineNumber {
                spec: spec_owned.to_owned(),
                line: num,
            });
        }
        if !rest.is_empty() {
            return Err(LineRangeError::InvalidSpec {
                spec: spec_owned.to_owned(),
                detail: format!("unexpected trailing text `{rest}`"),
            });
        }
        return Ok(num as usize);
    }

    if token.starts_with('/') {
        let scan_anchor = if scan_from_line_one { 1 } else { relative_base };
        return slash_search_line(token, spec_owned, view, scan_anchor);
    }

    Err(LineRangeError::InvalidSpec {
        spec: spec_owned.to_owned(),
        detail: format!("could not parse `{token}`"),
    })
}

fn relative_offset_line(
    token: &str,
    spec_owned: &str,
    base_1: i64,
) -> Result<usize, LineRangeError> {
    let (num, rest) = leading_decimal(&token[1..]).ok_or_else(|| LineRangeError::InvalidSpec {
        spec: spec_owned.to_owned(),
        detail: format!(
            "expected relative offset after `{}`",
            token.chars().next().unwrap_or('?')
        ),
    })?;
    if !rest.is_empty() {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: format!("unexpected trailing text `{rest}`"),
        });
    }
    if num == 0 {
        return Err(LineRangeError::EmptyRange {
            spec: spec_owned.to_owned(),
        });
    }
    let signed = if token.starts_with('-') { -num } else { num };
    let resolved = if signed > 0 {
        (base_1 as usize)
            .saturating_add(signed as usize)
            .saturating_sub(2)
    } else if signed == 0 {
        base_1 as usize
    } else {
        let shifted = base_1 + signed;
        if shifted > 0 {
            shifted as usize
        } else {
            1
        }
    };
    Ok(resolved)
}

fn slash_search_line(
    token: &str,
    spec_owned: &str,
    view: &TextView<'_>,
    scan_anchor: i64,
) -> Result<usize, LineRangeError> {
    let mut body = token;
    let mut scan_line_1 = scan_anchor;
    if scan_line_1 < 0 {
        if body.starts_with('^') {
            scan_line_1 = 1;
            body = body.trim_start_matches('^').trim_start();
        } else {
            scan_line_1 = -scan_line_1;
        }
    }
    let (pattern, rest) = delimited_slashes(body).ok_or_else(|| LineRangeError::InvalidSpec {
        spec: spec_owned.to_owned(),
        detail: "expected /pattern/".into(),
    })?;
    if !rest.is_empty() {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: format!("unexpected trailing text `{rest}`"),
        });
    }
    let line_index_0 = scan_line_1.saturating_sub(1) as usize;
    let start_byte = byte_offset_of_line(view.raw, line_index_0);
    let haystack = &view.raw.as_bytes()[start_byte..];
    let re = PosixBre::compile(&pattern).map_err(|detail| LineRangeError::InvalidRegex {
        pattern: pattern.clone(),
        detail,
    })?;
    let (rel_start, _) = re
        .find_from(haystack, 0)
        .ok_or_else(|| LineRangeError::RegexNoMatch {
            pattern,
            start_line: line_index_0.saturating_add(1),
        })?;
    Ok(one_based_line_at_byte(view.raw, start_byte + rel_start))
}

fn decode_symbol_range(
    spec: &str,
    spec_owned: &str,
    view: &TextView<'_>,
    line_count: usize,
    anchor_1: usize,
    matcher: Option<&FuncnameMatcher>,
) -> Result<DraftRange, LineRangeError> {
    let mut body = spec;
    let mut anchor = anchor_1;
    if body.starts_with('^') {
        anchor = 1;
        body = &body[1..];
    }
    if !body.starts_with(':') {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: "expected :symbol".into(),
        });
    }
    let (pattern, rest) =
        delimited_colon_pattern(&body[1..]).ok_or_else(|| LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: "expected :symbol".into(),
        })?;
    if !rest.is_empty() {
        return Err(LineRangeError::InvalidSpec {
            spec: spec_owned.to_owned(),
            detail: format!("unexpected trailing text `{rest}`"),
        });
    }

    let re =
        PosixBre::compile(&pattern).map_err(|detail| LineRangeError::FuncnameInvalidPattern {
            pattern: pattern.clone(),
            detail,
        })?;

    let start_index_0 = anchor.saturating_sub(1);
    let start_byte = byte_offset_of_line(view.raw, start_index_0.min(line_count.saturating_sub(1)));
    let header_byte =
        locate_func_header_after(view, start_byte, &re, matcher).ok_or_else(|| {
            LineRangeError::FuncnameNoMatch {
                pattern: pattern.clone(),
                start_line: anchor,
            }
        })?;

    let mut begin_idx = 0usize;
    while header_byte > byte_offset_of_line(view.raw, begin_idx) && begin_idx + 1 < line_count {
        begin_idx += 1;
    }
    if begin_idx >= line_count {
        return Err(LineRangeError::FuncnameAtEof { pattern });
    }

    let mut end_idx = begin_idx + 1;
    while end_idx < line_count {
        if is_func_line(view.line_text(end_idx), matcher) {
            break;
        }
        end_idx += 1;
    }

    Ok(DraftRange {
        start: begin_idx + 1,
        end: end_idx,
    })
}

fn locate_func_header_after(
    view: &TextView<'_>,
    start_byte: usize,
    re: &PosixBre,
    matcher: Option<&FuncnameMatcher>,
) -> Option<usize> {
    let bytes = view.raw.as_bytes();
    let mut pos = start_byte;
    while pos <= bytes.len() {
        let haystack = &bytes[pos..];
        let (rel_start, _) = re.find_from(haystack, 0)?;
        let match_start = pos + rel_start;
        let line_0 = one_based_line_at_byte(view.raw, match_start).saturating_sub(1);
        let (bol, _) = view.line_byte_range(line_0);
        if is_func_line(view.line_text(line_0), matcher) {
            return Some(bol);
        }
        pos = if line_0 + 1 < view.line_count() {
            byte_offset_of_line(view.raw, line_0 + 1)
        } else {
            bytes.len()
        };
    }
    None
}

fn finalize_inclusive_range(
    mut draft: DraftRange,
    line_count: usize,
    spec_owned: &str,
) -> Result<RangeInclusive<usize>, LineRangeError> {
    if draft.start == 0 {
        draft.start = 1;
    }
    if draft.end == 0 || draft.end > line_count {
        draft.end = line_count;
    }
    if draft.start > line_count && line_count > 0 {
        return Err(LineRangeError::OutOfRange {
            spec: spec_owned.to_owned(),
            lines: line_count,
        });
    }
    if line_count == 0 && (draft.start > 0 || draft.end > 0) {
        return Err(LineRangeError::OutOfRange {
            spec: spec_owned.to_owned(),
            lines: 0,
        });
    }
    if draft.start > draft.end {
        std::mem::swap(&mut draft.start, &mut draft.end);
    }
    Ok(draft.start..=draft.end)
}

fn split_first_comma(spec: &str) -> (&str, Option<&str>) {
    let mut in_slashes = false;
    let mut escaped = false;
    for (idx, c) in spec.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if c == '\\' {
            escaped = true;
            continue;
        }
        if c == '/' {
            in_slashes = !in_slashes;
            continue;
        }
        if c == ',' && !in_slashes {
            return (&spec[..idx], Some(&spec[idx + 1..]));
        }
    }
    (spec, None)
}

fn delimited_slashes(token: &str) -> Option<(String, &str)> {
    if !token.starts_with('/') {
        return None;
    }
    let mut term = 1usize;
    let bytes = token.as_bytes();
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
    let pattern = token[1..term].to_owned();
    Some((pattern, &token[term + 1..]))
}

fn delimited_colon_pattern(token: &str) -> Option<(String, &str)> {
    let mut term = 0usize;
    let bytes = token.as_bytes();
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
    Some((token[..term].to_owned(), &token[term..]))
}

fn leading_decimal(token: &str) -> Option<(i64, &str)> {
    let token = token.trim_start();
    let mut end = 0usize;
    for (i, c) in token.char_indices() {
        if c.is_ascii_digit() {
            end = i + c.len_utf8();
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    let num: i64 = token[..end].parse().ok()?;
    Some((num, &token[end..]))
}

fn byte_offset_of_line(content: &str, line_index_0: usize) -> usize {
    if line_index_0 == 0 {
        return 0;
    }
    let mut line = 0usize;
    for (idx, b) in content.bytes().enumerate() {
        if b == b'\n' {
            line += 1;
            if line == line_index_0 {
                return idx + 1;
            }
        }
    }
    content.len()
}

fn one_based_line_at_byte(content: &str, byte: usize) -> usize {
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
    fn bre_slash_pattern_treats_plus_as_literal() {
        let content = "aaa\na+\n";
        let r =
            resolve_line_range("/a+/", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 2..=2);
    }

    #[test]
    fn bre_slash_pattern_preserves_backslash_before_plus() {
        let content = "aaa\na+\n";
        let r =
            resolve_line_range("/a\\+/", content, "f.txt", &ConfigSet::default(), None, 1).unwrap();
        assert_eq!(r, 1..=2);
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
