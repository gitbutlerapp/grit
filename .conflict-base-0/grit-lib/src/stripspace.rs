//! Core logic for `git stripspace`.
//!
//! Provides whitespace stripping and comment-line prefixing transformations
//! that match Git's behaviour:
//!
//! - Strip trailing whitespace from every line.
//! - Collapse multiple consecutive blank lines into one.
//! - Remove leading and trailing blank lines.
//! - Ensure non-empty output ends with a newline.
//! - Optionally strip lines that start with a comment prefix string.
//! - Optionally prefix every line with the comment character.

/// Processing mode for [`process`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Strip trailing whitespace and collapse blank lines.
    Default,
    /// Same as [`Mode::Default`] but also remove comment lines.
    ///
    /// A comment line is any line whose first bytes match `comment_prefix`
    /// (e.g. `"#"`).
    StripComments(String),
}

/// Process `input` bytes according to `mode` and return the result.
///
/// # Parameters
///
/// - `input`: raw bytes read from stdin.
/// - `mode`: controls whether to strip, strip-and-remove-comments, or add comments.
///
/// # Returns
///
/// A `Vec<u8>` with the transformed content.  Returns an empty vector when the
/// input consists entirely of whitespace (in strip modes) or is itself empty.
///
/// # Examples
///
/// ```ignore
/// use grit_lib::stripspace::{process, Mode};
///
/// let out = process(b"hello   \n\n\nworld\n", &Mode::Default);
/// assert_eq!(out, b"hello\n\nworld\n");
///
/// let out = process(b"# comment\ntext\n", &Mode::StripComments("#".into()));
/// assert_eq!(out, b"text\n");
/// ```
#[must_use]
pub fn process(input: &[u8], mode: &Mode) -> Vec<u8> {
    match mode {
        Mode::Default => strip(input, None),
        Mode::StripComments(prefix) => strip(input, Some(prefix.as_str())),
    }
}

/// Returns a copy of `line` with trailing space/tab bytes removed.
///
/// The line is expected to end with `\n`; the newline is preserved (not
/// considered trailing whitespace).
fn strip_trailing(line: &[u8]) -> Vec<u8> {
    let nl_pos = line.iter().rposition(|&b| b == b'\n');
    let content_end = nl_pos.unwrap_or(line.len());
    let content = &line[..content_end];

    let trimmed_end = content
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map(|p| p + 1)
        .unwrap_or(0);

    let mut result = content[..trimmed_end].to_vec();
    if nl_pos.is_some() {
        result.push(b'\n');
    }
    result
}

/// Core strip implementation shared by [`Mode::Default`] and
/// [`Mode::StripComments`].
///
/// When `comment_prefix` is `Some(s)`, lines whose bytes begin with `s` are
/// discarded before any other processing.
fn strip(input: &[u8], comment_prefix: Option<&str>) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }

    // Ensure the data ends with a newline so every line is terminated.
    let owned;
    let data: &[u8] = if input.last() != Some(&b'\n') {
        owned = {
            let mut v = input.to_vec();
            v.push(b'\n');
            v
        };
        &owned
    } else {
        input
    };

    let mut result: Vec<u8> = Vec::new();
    let mut pending_blank: usize = 0;
    let mut saw_content = false;

    let mut pos = 0;
    while pos < data.len() {
        let next = data[pos..]
            .iter()
            .position(|&b| b == b'\n')
            .map(|p| pos + p + 1)
            .unwrap_or(data.len());
        let raw_line = &data[pos..next];
        pos = next;

        // Discard comment lines when requested.
        if let Some(prefix) = comment_prefix {
            if raw_line.starts_with(prefix.as_bytes()) {
                continue;
            }
        }

        // Strip trailing whitespace; the result ends with '\n'.
        let stripped = strip_trailing(raw_line);

        // A line that reduces to just '\n' is blank.
        if stripped == b"\n" {
            if saw_content {
                pending_blank += 1;
            }
            // Skip leading blank lines (before any real content).
            continue;
        }

        // Non-blank line: flush at most one pending blank, then emit the line.
        if saw_content && pending_blank > 0 {
            result.push(b'\n');
        }
        pending_blank = 0;
        saw_content = true;
        result.extend_from_slice(&stripped);
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Mode::Default ────────────────────────────────────────────────────────

    #[test]
    fn default_strips_trailing_whitespace() {
        let out = process(b"hello   \n", &Mode::Default);
        assert_eq!(out, b"hello\n");
    }

    #[test]
    fn default_collapses_consecutive_blank_lines() {
        let out = process(b"a\n\n\n\nb\n", &Mode::Default);
        assert_eq!(out, b"a\n\nb\n");
    }

    #[test]
    fn default_removes_leading_blank_lines() {
        let out = process(b"\n\n\ntext\n", &Mode::Default);
        assert_eq!(out, b"text\n");
    }

    #[test]
    fn default_removes_trailing_blank_lines() {
        let out = process(b"text\n\n\n", &Mode::Default);
        assert_eq!(out, b"text\n");
    }

    #[test]
    fn default_all_whitespace_yields_empty() {
        assert_eq!(process(b"   \n  \n\n", &Mode::Default), b"");
        assert_eq!(process(b"\n", &Mode::Default), b"");
        assert_eq!(process(b"", &Mode::Default), b"");
    }

    #[test]
    fn default_adds_trailing_newline_when_missing() {
        let out = process(b"text", &Mode::Default);
        assert_eq!(out, b"text\n");
    }

    #[test]
    fn default_preserves_leading_spaces_on_line() {
        let out = process(b"  indented\n", &Mode::Default);
        assert_eq!(out, b"  indented\n");
    }

    #[test]
    fn default_blank_lines_between_whitespace_only_lines() {
        // Lines with only spaces count as blank.
        let out = process(b"a\n   \n   \nb\n", &Mode::Default);
        assert_eq!(out, b"a\n\nb\n");
    }

    // ── Mode::StripComments ──────────────────────────────────────────────────

    #[test]
    fn strip_comments_removes_hash_lines() {
        // Comment lines are simply removed; no blank is inserted in their place.
        let out = process(b"text\n# comment\nmore\n", &Mode::StripComments("#".into()));
        assert_eq!(out, b"text\nmore\n");
    }

    #[test]
    fn strip_comments_keeps_non_comment_lines() {
        let out = process(b"# comment\n", &Mode::StripComments("#".into()));
        assert_eq!(out, b"");
    }

    #[test]
    fn strip_comments_multichar_prefix() {
        let out = process(
            b"// removed\nnormal line\n",
            &Mode::StripComments("//".into()),
        );
        assert_eq!(out, b"normal line\n");
    }
}
