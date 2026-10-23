//! Line comparison rules for `git diff -w`, `-b`, `--ignore-space-at-eol`, and `--ignore-blank-lines`.

use std::borrow::Cow;

use crate::ws::is_space_git;

/// Whitespace handling for line diffs ([`super::DiffOptions::whitespace`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct WhitespaceMode {
    /// `-w` / `--ignore-all-space`: drop every whitespace character when comparing lines.
    pub ignore_all_space: bool,
    /// `-b` / `--ignore-space-change`: collapse runs of whitespace to one space; trim trailing spaces.
    pub ignore_space_change: bool,
    /// `--ignore-space-at-eol`: trim trailing whitespace before the line terminator.
    pub ignore_space_at_eol: bool,
    /// `--ignore-blank-lines`: treat blank-line-only change records as ignorable (Git `xch->ignore`).
    pub ignore_blank_lines: bool,
}

impl WhitespaceMode {
    /// `-w` / `--ignore-all-space`.
    #[must_use]
    pub fn ignore_all_space() -> Self {
        Self {
            ignore_all_space: true,
            ..Self::default()
        }
    }

    /// `-b` / `--ignore-space-change`.
    #[must_use]
    pub fn ignore_space_change() -> Self {
        Self {
            ignore_space_change: true,
            ..Self::default()
        }
    }

    /// `--ignore-blank-lines`.
    #[must_use]
    pub fn ignore_blank_lines() -> Self {
        Self {
            ignore_blank_lines: true,
            ..Self::default()
        }
    }

    /// Whether any whitespace rule is enabled.
    #[must_use]
    pub fn is_active(self) -> bool {
        self.ignore_all_space
            || self.ignore_space_change
            || self.ignore_space_at_eol
            || self.ignore_blank_lines
    }

    /// Whether line text should be normalized before xdiff line hashing (`-w`, `-b`, `--ignore-space-at-eol`).
    #[must_use]
    pub fn normalizes_line_compare(self) -> bool {
        self.ignore_all_space || self.ignore_space_change || self.ignore_space_at_eol
    }

    /// Normalize one line body (without its trailing `\n`) for line-interner comparison.
    #[must_use]
    pub fn normalize_line_for_compare<'a>(&self, line: &'a str) -> Cow<'a, str> {
        if !self.normalizes_line_compare() {
            return Cow::Borrowed(line);
        }
        if self.ignore_all_space {
            return Cow::Owned(strip_git_whitespace_bytes(line));
        }
        let mut work = line.to_string();
        if self.ignore_space_at_eol {
            work = trim_trailing_git_whitespace(&work);
        }
        if self.ignore_space_change {
            work = normalize_ignore_space_change_line(&work);
        }
        Cow::Owned(work)
    }
}

/// Normalise one line like Git's `-b` / `--ignore-space-change` (ASCII ws only).
#[must_use]
pub fn normalize_ignore_space_change_line(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut result = String::with_capacity(line.len());
    let mut in_space = false;
    for &ch in bytes {
        if is_space_git(ch) {
            if !in_space {
                result.push(' ');
                in_space = true;
            }
        } else {
            result.push(ch as char);
            in_space = false;
        }
    }
    while result.ends_with(' ') {
        result.pop();
    }
    result
}

/// Strip Git whitespace bytes for `-w` (NBSP and other non-Git space bytes are kept).
#[must_use]
pub fn strip_git_whitespace_bytes(line: &str) -> String {
    let filtered: Vec<u8> = line.bytes().filter(|b| !is_space_git(*b)).collect();
    String::from_utf8(filtered).unwrap_or_else(|_| line.to_string())
}

fn trim_trailing_git_whitespace(line: &str) -> String {
    let bytes = line.as_bytes();
    let mut end = bytes.len();
    while end > 0 && is_space_git(bytes[end - 1]) {
        end -= 1;
    }
    if end == bytes.len() {
        return line.to_string();
    }
    String::from_utf8(bytes[..end].to_vec()).unwrap_or_else(|_| line.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ignore_space_change_collapses_runs() {
        assert_eq!(normalize_ignore_space_change_line("f o o"), "f o o");
        assert_eq!(normalize_ignore_space_change_line("a  b\tc  "), "a b c");
    }

    #[test]
    fn ignore_all_space_strips_git_whitespace_only() {
        let ws = WhitespaceMode::ignore_all_space();
        assert_eq!(ws.normalize_line_for_compare("f o o").as_ref(), "foo");
        let nbsp_line = "a\u{00a0}b";
        assert_eq!(
            ws.normalize_line_for_compare(nbsp_line).as_ref(),
            nbsp_line,
            "NBSP is not Git whitespace and must not be stripped by -w"
        );
    }

    #[test]
    fn nbsp_change_not_ignored_by_ignore_all_space_compare_key() {
        let ws = WhitespaceMode::ignore_all_space();
        let old_key = ws.normalize_line_for_compare("a\u{00a0}b");
        let new_key = ws.normalize_line_for_compare("ab");
        assert_ne!(old_key, new_key);
    }
}
