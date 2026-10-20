//! Pickaxe filters (`-S` / `-G`) for revision walks and diffs.
//!
//! `-S` keeps commits where the number of occurrences of a string (or BRE pattern with
//! `--pickaxe-regex`) differs between a file's pre- and post-image compared to the first
//! parent. `-G` keeps commits where added or removed lines match a regex (Git line-level diff).

use std::sync::Arc;

use regex::bytes::{Regex, RegexBuilder};

use crate::diff::structured::SideBytes;
use crate::diff::{diff_trees, DiffEntry};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, ObjectId};
use crate::pathspec::matches_pathspec_list;
use crate::repo::Repository;
use crate::revwalk::filter::{bre_translate_to_rust_regex, CommitFilterError};

/// Which pickaxe mode is active (`-S` or `-G`).
#[derive(Debug, Clone)]
pub enum PickaxeKind {
    /// Occurrence count change (`git log -S` / `git diff -S`).
    Occurrences {
        /// Needle string or BRE pattern when [`Pickaxe::pickaxe_regex`] is set.
        needle: String,
    },
    /// Line-level regex on added/removed lines (`git log -G`).
    LineGrep(Regex),
}

/// Compiled pickaxe filter for [`crate::rev_list::rev_list`].
#[derive(Debug, Clone)]
pub struct Pickaxe {
    kind: PickaxeKind,
    ignore_case: bool,
    /// When true with [`PickaxeKind::Occurrences`], interpret the needle as a BRE (`--pickaxe-regex`).
    pickaxe_regex: bool,
}

impl Pickaxe {
    /// `-S` / occurrence-count pickaxe (literal string unless [`Self::pickaxe_regex`]).
    #[must_use]
    pub fn occurrences(needle: impl Into<String>) -> Self {
        Self {
            kind: PickaxeKind::Occurrences {
                needle: needle.into(),
            },
            ignore_case: false,
            pickaxe_regex: false,
        }
    }

    /// `-G` / line-level regex pickaxe (Git default BRE).
    ///
    /// # Errors
    ///
    /// Returns [`PickaxeError::InvalidPattern`] when the pattern does not compile.
    pub fn line_grep(pattern: &str, ignore_case: bool) -> std::result::Result<Self, PickaxeError> {
        let regex = compile_line_grep_regex(pattern, ignore_case)?;
        Ok(Self {
            kind: PickaxeKind::LineGrep(regex),
            ignore_case,
            pickaxe_regex: false,
        })
    }

    /// Case-insensitive matching (Git `-i` / `--regexp-ignore-case`).
    #[must_use]
    pub fn ignore_case(mut self, on: bool) -> Self {
        self.ignore_case = on;
        self
    }

    /// Treat `-S` needle as a BRE (`--pickaxe-regex`; only valid with occurrence mode).
    #[must_use]
    pub fn pickaxe_regex(mut self, on: bool) -> Self {
        self.pickaxe_regex = on;
        self
    }

    /// Whether this filter requires per-commit tree diffs.
    #[must_use]
    pub fn is_active(&self) -> bool {
        true
    }

    /// Shared handle for [`crate::rev_list::RevListOptions`].
    #[must_use]
    pub fn into_shared(self) -> Arc<Self> {
        Arc::new(self)
    }

    /// Returns true when `commit` would be kept by `git log` with this pickaxe vs its first parent.
    ///
    /// Merge commits compare only to the first parent. Pathspecs limit which tree entries are
    /// considered. Binary files are skipped for `-G` (unless textconv is added later); `-S` searches
    /// binary blobs.
    ///
    /// # Errors
    ///
    /// Propagates object read and diff errors.
    pub fn matches_commit(
        &self,
        repo: &Repository,
        commit: ObjectId,
        paths: &[String],
    ) -> Result<bool> {
        let data = repo.odb().read(&commit)?.data;
        let commit = parse_commit(&data)?;
        let parents = commit.parents;
        let parent_tree = if parents.is_empty() {
            None
        } else {
            let parent_data = repo.odb().read(&parents[0])?.data;
            Some(parse_commit(&parent_data)?.tree)
        };
        let entries = diff_trees(repo.odb(), parent_tree.as_ref(), Some(&commit.tree), "")?;
        let odb = repo.odb();
        for entry in entries {
            if !entry_matches_pathspecs(&entry, paths) {
                continue;
            }
            if entry.old_oid == entry.new_oid && entry.old_mode == entry.new_mode {
                continue;
            }
            if self.filepair_matches(odb, &entry)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn filepair_matches(&self, odb: &crate::odb::Odb, entry: &DiffEntry) -> Result<bool> {
        let (old_bytes, old_bin) = blob_side(odb, entry.old_mode.as_str(), &entry.old_oid)?;
        let (new_bytes, new_bin) = blob_side(odb, entry.new_mode.as_str(), &entry.new_oid)?;
        match &self.kind {
            PickaxeKind::Occurrences { needle } => Ok(has_occurrence_change(
                old_bytes.data.as_slice(),
                new_bytes.data.as_slice(),
                needle,
                self.ignore_case,
                self.pickaxe_regex,
            )?),
            PickaxeKind::LineGrep(regex) => {
                if old_bin || new_bin {
                    return Ok(false);
                }
                Ok(line_grep_match(
                    old_bytes.text().as_str(),
                    new_bytes.text().as_str(),
                    regex,
                ))
            }
        }
    }
}

/// Invalid pickaxe pattern.
#[derive(Debug, thiserror::Error)]
pub enum PickaxeError {
    /// Pattern compilation failed.
    #[error("invalid pickaxe pattern `{pattern}`: {detail}")]
    InvalidPattern {
        /// Original pattern text.
        pattern: String,
        /// Underlying error message.
        detail: String,
    },
}

impl From<PickaxeError> for Error {
    fn from(value: PickaxeError) -> Self {
        match value {
            PickaxeError::InvalidPattern { pattern, detail } => {
                Error::Message(format!("invalid pickaxe pattern `{pattern}`: {detail}"))
            }
        }
    }
}

impl From<CommitFilterError> for PickaxeError {
    fn from(value: CommitFilterError) -> Self {
        match value {
            CommitFilterError::InvalidPattern { pattern, detail } => {
                PickaxeError::InvalidPattern { pattern, detail }
            }
        }
    }
}

fn compile_line_grep_regex(
    pattern: &str,
    ignore_case: bool,
) -> std::result::Result<Regex, PickaxeError> {
    let rust_pat =
        bre_translate_to_rust_regex(pattern).map_err(|detail| PickaxeError::InvalidPattern {
            pattern: pattern.to_owned(),
            detail,
        })?;
    let with_newline = format!("(?m){rust_pat}");
    RegexBuilder::new(&with_newline)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| PickaxeError::InvalidPattern {
            pattern: pattern.to_owned(),
            detail: e.to_string(),
        })
}

fn compile_occurrence_regex(
    pattern: &str,
    ignore_case: bool,
) -> std::result::Result<Regex, PickaxeError> {
    let rust_pat =
        bre_translate_to_rust_regex(pattern).map_err(|detail| PickaxeError::InvalidPattern {
            pattern: pattern.to_owned(),
            detail,
        })?;
    RegexBuilder::new(&rust_pat)
        .case_insensitive(ignore_case)
        .build()
        .map_err(|e| PickaxeError::InvalidPattern {
            pattern: pattern.to_owned(),
            detail: e.to_string(),
        })
}

fn entry_matches_pathspecs(entry: &DiffEntry, paths: &[String]) -> bool {
    if paths.is_empty() {
        return true;
    }
    let path = entry.path();
    if path.is_empty() {
        return false;
    }
    matches_pathspec_list(path, paths)
}

fn has_occurrence_change(
    old: &[u8],
    new: &[u8],
    needle: &str,
    ignore_case: bool,
    as_regex: bool,
) -> Result<bool> {
    let c1 = count_occurrences(old, needle, ignore_case, as_regex)?;
    let c2 = count_occurrences(new, needle, ignore_case, as_regex)?;
    Ok(c1 != c2)
}

fn count_occurrences(
    haystack: &[u8],
    needle: &str,
    ignore_case: bool,
    as_regex: bool,
) -> Result<usize> {
    if haystack.is_empty() {
        return Ok(0);
    }
    if as_regex {
        let re = compile_occurrence_regex(needle, ignore_case)?;
        let mut count = 0usize;
        let mut start = 0usize;
        while start <= haystack.len() {
            let slice = &haystack[start..];
            if slice.is_empty() {
                break;
            }
            match re.find(slice) {
                Some(m) => {
                    count += 1;
                    let adv = m.end();
                    if adv == 0 {
                        start += 1;
                    } else {
                        start += adv;
                    }
                }
                None => break,
            }
        }
        return Ok(count);
    }
    if ignore_case {
        let h = String::from_utf8_lossy(haystack).to_lowercase();
        let n = needle.to_lowercase();
        return Ok(count_substr(h.as_bytes(), n.as_bytes()));
    }
    Ok(count_substr(haystack, needle.as_bytes()))
}

fn count_substr(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() {
        return 0;
    }
    let mut count = 0usize;
    let mut start = 0usize;
    while start + needle.len() <= haystack.len() {
        if haystack[start..].starts_with(needle) {
            count += 1;
            start += needle.len();
        } else {
            start += 1;
        }
    }
    count
}

fn blob_side(odb: &crate::odb::Odb, mode: &str, oid: &ObjectId) -> Result<(SideBytes, bool)> {
    if mode == "160000" {
        let line = if *oid == ObjectId::zero() {
            String::new()
        } else {
            format!("Subproject commit {}\n", oid.to_hex())
        };
        return Ok((SideBytes::from_bytes(line.into_bytes()), false));
    }
    if mode == "000000" || *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let data = odb.read(oid)?.data;
    let binary = data.iter().take(8000).any(|&b| b == 0);
    Ok((SideBytes::from_bytes(data), binary))
}

fn line_grep_match(old_text: &str, new_text: &str, regex: &Regex) -> bool {
    use similar::{ChangeTag, TextDiff};
    let diff = TextDiff::from_lines(old_text, new_text);
    for change in diff.iter_all_changes() {
        let line = change.value();
        let check = match change.tag() {
            ChangeTag::Delete | ChangeTag::Insert => line.as_bytes(),
            ChangeTag::Equal => continue,
        };
        if regex.is_match(check) {
            return true;
        }
    }
    false
}
