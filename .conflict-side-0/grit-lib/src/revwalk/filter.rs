//! Commit message and identity filters for revision walks (`--author`, `--grep`, …).
//!
//! Patterns use Git's default POSIX basic regular expression (BRE) syntax unless
//! [`CommitFilterBuilder::fixed_strings`] is set. Callers supply an optional
//! [`MailmapTable`] loaded explicitly
//! (for example via [`load_mailmap_table`](crate::config::mailmap::load_mailmap_table));
//! the filter never reads repository config on its own.
//!
//! ```
//! use grit_lib::config::mailmap::MailmapTable;
//! use grit_lib::objects::ObjectId;
//! use grit_lib::repo::{InitOptions, Repository};
//! use grit_lib::revwalk::filter::{CommitFilter, CommitFilterBuilder};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let dir = tempfile::tempdir()?;
//! let repo = Repository::init(dir.path(), &InitOptions::default())?;
//! let filter = CommitFilterBuilder::new()
//!     .author("Alice")
//!     .build()?;
//! let _matches = filter.is_active();
//! # let _ = (repo, filter);
//! # Ok(())
//! # }
//! ```

use std::borrow::Cow;
use std::sync::Arc;

use regex::bytes::{Regex, RegexBuilder, RegexSet, RegexSetBuilder};

use crate::config::mailmap::{map_contact_table, parse_contact, MailmapTable};
use crate::error::{Error, Result};
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;

/// A single user-supplied match pattern (BRE or fixed string).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pattern(String);

impl Pattern {
    /// Wrap a pattern string from the caller.
    #[must_use]
    pub fn new(raw: impl Into<String>) -> Self {
        Self(raw.into())
    }

    /// Borrow the raw pattern text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl From<String> for Pattern {
    fn from(s: String) -> Self {
        Self(s)
    }
}

impl From<&str> for Pattern {
    fn from(s: &str) -> Self {
        Self(s.to_owned())
    }
}

/// Invalid commit-filter pattern or compilation failure.
#[derive(Debug, thiserror::Error)]
pub enum CommitFilterError {
    /// A pattern could not be compiled.
    #[error("invalid commit filter pattern `{pattern}`: {detail}")]
    InvalidPattern {
        /// Original pattern text.
        pattern: String,
        /// Underlying regex error message.
        detail: String,
    },
}

/// Compiled commit filter applied during [`crate::rev_list::rev_list`].
#[derive(Debug, Clone)]
pub struct CommitFilter {
    inner: Arc<CommitFilterInner>,
}

#[derive(Debug)]
struct CommitFilterInner {
    author: Option<RegexSet>,
    committer: Option<RegexSet>,
    grep: Vec<Regex>,
    all_match: bool,
    invert_grep: bool,
    mailmap: Option<MailmapTable>,
}

impl CommitFilter {
    /// Start building a filter.
    #[must_use]
    pub fn builder() -> CommitFilterBuilder {
        CommitFilterBuilder::new()
    }

    /// Returns true when any author, committer, or grep pattern is configured.
    #[must_use]
    pub fn is_active(&self) -> bool {
        let i = &self.inner;
        i.author.is_some() || i.committer.is_some() || !i.grep.is_empty()
    }

    /// Test whether `oid` satisfies this filter (author/committer/grep/mailmap rules).
    ///
    /// # Errors
    ///
    /// Returns repository and object read errors from the ODB.
    pub fn matches(&self, repo: &Repository, oid: ObjectId) -> Result<bool> {
        let object = repo.odb().read(&oid)?;
        if object.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "expected commit object for {oid}, found {:?}",
                object.kind
            )));
        }
        Ok(self.matches_bytes(&object.data))
    }

    fn matches_bytes(&self, raw: &[u8]) -> bool {
        let inner = &self.inner;
        let headers = scan_commit_headers(raw);

        if let Some(set) = &inner.author {
            let ident = ident_for_pattern_match(headers.author, inner.mailmap.as_ref());
            if !set.is_match(ident.as_ref()) {
                return false;
            }
        }

        if let Some(set) = &inner.committer {
            let ident = ident_for_pattern_match(headers.committer, inner.mailmap.as_ref());
            if !set.is_match(ident.as_ref()) {
                return false;
            }
        }

        if !inner.grep.is_empty() {
            let body = commit_message_body(raw);
            let grep_ok = if inner.all_match {
                inner.grep.iter().all(|re| re.is_match(body))
            } else {
                inner.grep.iter().any(|re| re.is_match(body))
            };
            let grep_ok = if inner.invert_grep { !grep_ok } else { grep_ok };
            if !grep_ok {
                return false;
            }
        }

        true
    }
}

/// Builder for [`CommitFilter`].
#[derive(Debug, Clone, Default)]
pub struct CommitFilterBuilder {
    author: Vec<Pattern>,
    committer: Vec<Pattern>,
    grep: Vec<Pattern>,
    ignore_case: bool,
    all_match: bool,
    invert_grep: bool,
    fixed_strings: bool,
    mailmap: Option<MailmapTable>,
}

impl CommitFilterBuilder {
    /// Empty builder.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an `--author` pattern (multiple patterns OR together).
    #[must_use]
    pub fn author(mut self, pattern: impl Into<Pattern>) -> Self {
        self.author.push(pattern.into());
        self
    }

    /// Add a `--committer` pattern (multiple patterns OR together).
    #[must_use]
    pub fn committer(mut self, pattern: impl Into<Pattern>) -> Self {
        self.committer.push(pattern.into());
        self
    }

    /// Add a `--grep` pattern (OR unless [`Self::all_match`], then AND).
    #[must_use]
    pub fn grep(mut self, pattern: impl Into<Pattern>) -> Self {
        self.grep.push(pattern.into());
        self
    }

    /// Case-insensitive matching for all pattern groups (`-i`).
    #[must_use]
    pub fn ignore_case(mut self, yes: bool) -> Self {
        self.ignore_case = yes;
        self
    }

    /// Require every `--grep` pattern to match (`--all-match`).
    #[must_use]
    pub fn all_match(mut self, yes: bool) -> Self {
        self.all_match = yes;
        self
    }

    /// Invert the `--grep` group only (`--invert-grep`).
    #[must_use]
    pub fn invert_grep(mut self, yes: bool) -> Self {
        self.invert_grep = yes;
        self
    }

    /// Treat patterns as fixed strings (`--fixed-strings` / `-F`).
    #[must_use]
    pub fn fixed_strings(mut self, yes: bool) -> Self {
        self.fixed_strings = yes;
        self
    }

    /// Optional mailmap table for author/committer identity mapping.
    #[must_use]
    pub fn mailmap(mut self, table: MailmapTable) -> Self {
        self.mailmap = Some(table);
        self
    }

    /// Compile patterns into a [`CommitFilter`].
    ///
    /// # Errors
    ///
    /// Returns [`CommitFilterError::InvalidPattern`] when any pattern fails to compile.
    pub fn build(self) -> std::result::Result<CommitFilter, CommitFilterError> {
        let author = compile_pattern_set(&self.author, self.ignore_case, self.fixed_strings)?;
        let committer = compile_pattern_set(&self.committer, self.ignore_case, self.fixed_strings)?;
        let mut grep = Vec::with_capacity(self.grep.len());
        for pat in &self.grep {
            grep.push(compile_grep_pattern(
                pat.as_str(),
                self.ignore_case,
                self.fixed_strings,
            )?);
        }
        Ok(CommitFilter {
            inner: Arc::new(CommitFilterInner {
                author,
                committer,
                grep,
                all_match: self.all_match,
                invert_grep: self.invert_grep,
                mailmap: self.mailmap,
            }),
        })
    }
}

struct CommitHeaderFields<'a> {
    author: Option<&'a [u8]>,
    committer: Option<&'a [u8]>,
}

fn commit_message_body(raw: &[u8]) -> &[u8] {
    raw.windows(2)
        .position(|w| w == b"\n\n")
        .map(|pos| &raw[pos + 2..])
        .unwrap_or(&[])
}

fn scan_commit_headers(raw: &[u8]) -> CommitHeaderFields<'_> {
    let mut author = None;
    let mut committer = None;
    let mut line_start = 0usize;
    for (i, &b) in raw.iter().enumerate() {
        if b == b'\n' {
            if line_start == i {
                break;
            }
            let line = &raw[line_start..i];
            if let Some(rest) = line.strip_prefix(b"author ") {
                author = Some(rest);
            } else if let Some(rest) = line.strip_prefix(b"committer ") {
                committer = Some(rest);
            }
            line_start = i + 1;
        }
    }
    CommitHeaderFields { author, committer }
}

fn trim_end_ascii_ws(ident: &[u8]) -> &[u8] {
    let end = ident
        .iter()
        .rposition(|&b| b != b' ' && b != b'\t')
        .map_or(0, |i| i + 1);
    &ident[..end]
}

fn ident_without_timestamp_bytes(ident: &[u8]) -> &[u8] {
    let Some(gt) = ident.iter().rposition(|&b| b == b'>') else {
        return trim_end_ascii_ws(ident);
    };
    trim_end_ascii_ws(&ident[..=gt])
}

/// Identity bytes matched against `--author` / `--committer` (timestamp stripped).
fn ident_for_pattern_match<'a>(
    ident: Option<&'a [u8]>,
    mailmap: Option<&MailmapTable>,
) -> Cow<'a, [u8]> {
    let Some(ident) = ident else {
        return Cow::Borrowed(b"");
    };
    let bare = ident_without_timestamp_bytes(ident);
    let Some(table) = mailmap else {
        return Cow::Borrowed(bare);
    };
    // Mailmap rules are text; non-UTF-8 idents keep raw bytes (same as matching without mailmap).
    let Ok(bare_str) = std::str::from_utf8(bare) else {
        return Cow::Borrowed(bare);
    };
    let (name, email) = parse_contact(bare_str);
    let (name, email) = map_contact_table(name.as_deref(), email.as_deref(), table);
    if email.is_empty() {
        Cow::Owned(name.into_bytes())
    } else if name.is_empty() {
        Cow::Owned(format!("<{email}>").into_bytes())
    } else {
        Cow::Owned(format!("{name} <{email}>").into_bytes())
    }
}

fn compile_pattern_set(
    patterns: &[Pattern],
    ignore_case: bool,
    fixed_strings: bool,
) -> std::result::Result<Option<RegexSet>, CommitFilterError> {
    if patterns.is_empty() {
        return Ok(None);
    }
    let mut rust_patterns = Vec::with_capacity(patterns.len());
    for pat in patterns {
        rust_patterns.push(pattern_to_rust_regex(
            pat.as_str(),
            ignore_case,
            fixed_strings,
        )?);
    }
    RegexSetBuilder::new(&rust_patterns)
        .case_insensitive(ignore_case)
        .build()
        .map(Some)
        .map_err(|e| CommitFilterError::InvalidPattern {
            pattern: patterns
                .first()
                .map_or_else(String::new, |p| p.as_str().to_owned()),
            detail: e.to_string(),
        })
}

fn compile_grep_pattern(
    pattern: &str,
    ignore_case: bool,
    fixed_strings: bool,
) -> std::result::Result<Regex, CommitFilterError> {
    let rust_pat = pattern_to_rust_regex(pattern, ignore_case, fixed_strings)?;
    let with_newline = format!("(?m){rust_pat}");
    RegexBuilder::new(&with_newline)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| CommitFilterError::InvalidPattern {
            pattern: pattern.to_owned(),
            detail: e.to_string(),
        })
}

fn pattern_to_rust_regex(
    pattern: &str,
    _ignore_case: bool,
    fixed_strings: bool,
) -> std::result::Result<String, CommitFilterError> {
    if fixed_strings {
        return Ok(regex::escape(pattern));
    }
    bre_translate_to_rust_regex(pattern).map_err(|detail| CommitFilterError::InvalidPattern {
        pattern: pattern.to_owned(),
        detail,
    })
}

/// Translate a Git BRE pattern into Rust `regex` syntax (not including `(?m)`).
pub(crate) fn bre_translate_to_rust_regex(pattern: &str) -> std::result::Result<String, String> {
    let mut out = String::with_capacity(pattern.len() + 8);
    let chars: Vec<char> = pattern.chars().collect();
    let mut i = 0usize;
    let mut in_bracket = false;

    while i < chars.len() {
        if in_bracket {
            if chars[i] == ']' && i > 0 {
                out.push(']');
                in_bracket = false;
                i += 1;
            } else if chars[i] == '[' {
                out.push('[');
                i += 1;
            } else if chars[i] == '\\' {
                if i + 1 < chars.len() {
                    let next = chars[i + 1];
                    out.push('\\');
                    out.push(next);
                    i += 2;
                } else {
                    out.push('\\');
                    i += 1;
                }
            } else {
                out.push(chars[i]);
                i += 1;
            }
            continue;
        }

        if chars[i] == '[' {
            out.push('[');
            in_bracket = true;
            i += 1;
            if i < chars.len() && (chars[i] == '^' || chars[i] == '!') {
                out.push(chars[i]);
                i += 1;
            }
            if i < chars.len() && chars[i] == ']' {
                out.push(']');
                i += 1;
            }
            continue;
        }

        if chars[i] == '\\' && i + 1 < chars.len() {
            match chars[i + 1] {
                '(' => {
                    out.push('(');
                    i += 2;
                }
                ')' => {
                    out.push(')');
                    i += 2;
                }
                '|' => {
                    out.push('|');
                    i += 2;
                }
                '+' => {
                    out.push('+');
                    i += 2;
                }
                '?' => {
                    out.push('?');
                    i += 2;
                }
                '{' => {
                    out.push('{');
                    i += 2;
                }
                '}' => {
                    out.push('}');
                    i += 2;
                }
                _ => {
                    out.push('\\');
                    out.push(chars[i + 1]);
                    i += 2;
                }
            }
            continue;
        }

        if matches!(chars[i], '(' | ')' | '|' | '+' | '?' | '{' | '}') {
            out.push('\\');
            out.push(chars[i]);
            i += 1;
            continue;
        }

        out.push(chars[i]);
        i += 1;
    }

    if in_bracket {
        return Err("unterminated bracket expression".to_owned());
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bre_metachars_become_literals_or_operators() {
        assert_eq!(
            bre_translate_to_rust_regex("fix(parser)").unwrap(),
            r"fix\(parser\)"
        );
        assert_eq!(bre_translate_to_rust_regex("a+b").unwrap(), r"a\+b");
        assert_eq!(bre_translate_to_rust_regex(r"\(x\|y\)").unwrap(), "(x|y)");
    }

    #[test]
    fn bre_bracket_class_preserved() {
        assert_eq!(bre_translate_to_rust_regex("[ab]").unwrap(), "[ab]");
        assert_eq!(bre_translate_to_rust_regex("[^ab]").unwrap(), "[^ab]");
    }

    #[test]
    fn matcher_author_or_and_grep() {
        let filter = CommitFilterBuilder::new()
            .author("Alice")
            .grep("fix")
            .build()
            .unwrap();
        let commit =
            b"tree abc\nauthor Alice <a@b.c> 1 +0000\ncommitter Alice <a@b.c> 1 +0000\n\nfix bug\n";
        assert!(filter.matches_bytes(commit));
        let other =
            b"tree abc\nauthor Bob <b@b.c> 1 +0000\ncommitter Bob <b@b.c> 1 +0000\n\nfix bug\n";
        assert!(!filter.matches_bytes(other));
    }

    #[test]
    fn grep_all_match_and_invert() {
        let both = CommitFilterBuilder::new()
            .grep("fix")
            .grep("bug")
            .all_match(true)
            .build()
            .unwrap();
        let msg = b"tree t\nauthor A <a> 1 +0000\ncommitter A <a> 1 +0000\n\nfix bug\n";
        assert!(both.matches_bytes(msg));
        let partial = b"tree t\nauthor A <a> 1 +0000\ncommitter A <a> 1 +0000\n\nother only\n";
        assert!(!both.matches_bytes(partial));

        let invert = CommitFilterBuilder::new()
            .grep("fix")
            .invert_grep(true)
            .build()
            .unwrap();
        assert!(!invert.matches_bytes(msg));
        assert!(invert.matches_bytes(partial));
    }

    #[test]
    fn fixed_strings_escape_regex() {
        let filter = CommitFilterBuilder::new()
            .grep("a+b")
            .fixed_strings(true)
            .build()
            .unwrap();
        let hit = b"tree t\nauthor A <a> 1 +0000\ncommitter A <a> 1 +0000\n\ncontains a+b\n";
        assert!(filter.matches_bytes(hit));
        let miss = b"tree t\nauthor A <a> 1 +0000\ncommitter A <a> 1 +0000\n\naaaab\n";
        assert!(!filter.matches_bytes(miss));
    }

    #[test]
    fn non_utf8_author_name_matches_email_pattern() {
        let filter = CommitFilterBuilder::new()
            .author("alice@example.com")
            .build()
            .unwrap();
        let commit = b"tree abc\nauthor \xFFAlice <alice@example.com> 1 +0000\ncommitter C <c@x.com> 1 +0000\n\nm\n";
        assert!(filter.matches_bytes(commit));
    }

    #[test]
    fn mailmap_applied_to_author_match() {
        let mut table = MailmapTable::default();
        crate::config::mailmap::read_mailmap_string(
            &mut table,
            "Proper Name <proper@email.com> <old@email.com>",
        );
        let filter = CommitFilterBuilder::new()
            .author("Proper Name")
            .mailmap(table)
            .build()
            .unwrap();
        let raw = b"tree t\nauthor Wrong <old@email.com> 1 +0000\ncommitter Wrong <old@email.com> 1 +0000\n\nm\n";
        assert!(filter.matches_bytes(raw));
    }
}
