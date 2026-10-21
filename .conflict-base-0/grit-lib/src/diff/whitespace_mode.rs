//! Line comparison rules for `git diff -w`, `-b`, `--ignore-space-at-eol`, and `--ignore-blank-lines`.

use std::borrow::Cow;

/// Whitespace handling for line diffs ([`super::DiffOptions::whitespace`]).
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
            let stripped: String = line.chars().filter(|c| !c.is_whitespace()).collect();
            return Cow::Owned(stripped);
        }
        let mut work = line.to_string();
        if self.ignore_space_at_eol {
            work = trim_trailing_whitespace_line(&work);
        }
        if self.ignore_space_change {
            work = normalize_ignore_space_change_line(&work);
        }
        Cow::Owned(work)
    }
}

/// Normalise one line like Git's `-b` / `--ignore-space-change`.
#[must_use]
pub fn normalize_ignore_space_change_line(line: &str) -> String {
    let mut result = String::with_capacity(line.len());
    let mut in_space = false;
    for c in line.chars() {
        if c.is_whitespace() {
            if !in_space {
                result.push(' ');
                in_space = true;
            }
        } else {
            result.push(c);
            in_space = false;
        }
    }
    while result.ends_with(' ') {
        result.pop();
    }
    result
}

fn trim_trailing_whitespace_line(line: &str) -> String {
    line.trim_end_matches(|c: char| c.is_whitespace())
        .to_string()
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
    fn ignore_all_space_strips_everything() {
        let ws = WhitespaceMode {
            ignore_all_space: true,
            ..WhitespaceMode::default()
        };
        assert_eq!(ws.normalize_line_for_compare("f o o").as_ref(), "foo");
    }
}
