//! Pickaxe filters (`-S` / `-G`) for revision walks and diffs.
//!
//! `-S` keeps commits where the number of occurrences of a string (or ERE pattern with
//! `--pickaxe-regex`) differs between a file's pre- and post-image compared to the first
//! parent. `-G` keeps commits where added or removed lines match a regex (Git line-level diff).
//! Merge commits are omitted unless combined merge diffs are requested (matches default `git log`).

use std::collections::HashMap;
use std::sync::Arc;

use crate::diff::posix_ere::PosixEre;
use crate::diff::structured::SideBytes;
use crate::diff::{diff_trees, DiffEntry};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, ObjectId};
use crate::pathspec::matches_pathspec_list;
use crate::repo::Repository;

/// Which pickaxe mode is active (`-S` or `-G`).
#[derive(Debug, Clone)]
pub enum PickaxeKind {
    /// Occurrence count change (`git log -S` / `git diff -S`).
    Occurrences {
        /// Needle string when not using [`Pickaxe::pickaxe_regex`].
        needle: String,
    },
    /// Line-level regex on added/removed lines (`git log -G`).
    LineGrep,
}

/// Compiled pickaxe filter for [`crate::rev_list::rev_list`].
#[derive(Debug, Clone)]
pub struct Pickaxe {
    kind: PickaxeKind,
    ignore_case: bool,
    /// When true with [`PickaxeKind::Occurrences`], interpret the needle as ERE (`--pickaxe-regex`).
    pickaxe_regex: bool,
    /// POSIX extended regex (`REG_EXTENDED | REG_NEWLINE`) for `-G` and `-S --pickaxe-regex`.
    ere: Option<PosixEre>,
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
            ere: None,
        }
    }

    /// `-G` / line-level regex pickaxe (Git `REG_EXTENDED | REG_NEWLINE`).
    ///
    /// # Errors
    ///
    /// Returns [`PickaxeError::InvalidPattern`] when the pattern does not compile.
    pub fn line_grep(pattern: &str, ignore_case: bool) -> std::result::Result<Self, PickaxeError> {
        let ere = compile_pickaxe_ere(pattern, ignore_case)?;
        Ok(Self {
            kind: PickaxeKind::LineGrep,
            ignore_case,
            pickaxe_regex: false,
            ere: Some(ere),
        })
    }

    /// Case-insensitive matching (Git `-i` / `--regexp-ignore-case`).
    #[must_use]
    pub fn ignore_case(mut self, on: bool) -> Self {
        self.ignore_case = on;
        self
    }

    /// Treat `-S` needle as an ERE (`--pickaxe-regex`; only valid with occurrence mode).
    #[must_use]
    pub fn pickaxe_regex(mut self, on: bool) -> Self {
        self.pickaxe_regex = on;
        self
    }

    /// Finalize a pickaxe built with [`Self::occurrences`] (compiles ERE when requested).
    ///
    /// # Errors
    ///
    /// Returns [`PickaxeError::InvalidPattern`] when `--pickaxe-regex` is set and the needle
    /// does not compile as ERE.
    pub fn build(mut self) -> std::result::Result<Self, PickaxeError> {
        if self.pickaxe_regex {
            let PickaxeKind::Occurrences { ref needle } = self.kind else {
                return Err(PickaxeError::InvalidPattern {
                    pattern: String::new(),
                    detail: "--pickaxe-regex cannot be used with -G".into(),
                });
            };
            self.ere = Some(compile_pickaxe_ere(needle, self.ignore_case)?);
        }
        Ok(self)
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
    /// Merge commits are skipped (Git default without combined merge diffs). Non-merge commits
    /// compare to the first parent only. Pathspecs limit which tree entries are considered.
    /// Binary files are skipped for `-G`; `-S` searches binary blobs.
    ///
    /// # Errors
    ///
    /// Propagates object read and diff errors.
    pub fn matches_commit(
        &self,
        repo: &Repository,
        commit_oid: ObjectId,
        paths: &[String],
    ) -> Result<bool> {
        let data = repo.odb().read(&commit_oid)?.data;
        let commit = parse_commit(&data)?;
        if commit.parents.len() > 1 {
            return Ok(false);
        }
        let parent_tree = if let Some(&parent_oid) = commit.parents.first() {
            let parent_data = repo.odb().read(&parent_oid)?.data;
            let parent = parse_commit(&parent_data)?;
            if parent.tree == commit.tree {
                return Ok(false);
            }
            Some(parent.tree)
        } else {
            None
        };
        self.matches_trees(repo, parent_tree.as_ref(), &commit.tree, paths, None)
    }

    /// Pickaxe match using pre-resolved parent and commit trees (avoids re-reading commits).
    ///
    /// `parent_tree` is `None` for root commits. Merge commits must be filtered out by the caller.
    pub(crate) fn matches_trees(
        &self,
        repo: &Repository,
        parent_tree: Option<&ObjectId>,
        commit_tree: &ObjectId,
        paths: &[String],
        mut blob_occurrence_cache: Option<&mut HashMap<ObjectId, usize>>,
    ) -> Result<bool> {
        let entries = diff_trees(repo.odb(), parent_tree, Some(commit_tree), "")?;
        let odb = repo.odb();
        for entry in entries {
            if !entry_matches_pathspecs(&entry, paths) {
                continue;
            }
            if entry.old_oid == entry.new_oid && entry.old_mode == entry.new_mode {
                continue;
            }
            if self.filepair_matches(odb, &entry, &mut blob_occurrence_cache)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn filepair_matches(
        &self,
        odb: &crate::odb::Odb,
        entry: &DiffEntry,
        blob_occurrence_cache: &mut Option<&mut HashMap<ObjectId, usize>>,
    ) -> Result<bool> {
        match &self.kind {
            PickaxeKind::Occurrences { needle } => {
                let c1 = blob_occurrence_count(
                    odb,
                    &entry.old_oid,
                    entry.old_mode.as_str(),
                    needle,
                    self.ignore_case,
                    self.ere.as_ref(),
                    blob_occurrence_cache.as_deref_mut(),
                )?;
                let c2 = blob_occurrence_count(
                    odb,
                    &entry.new_oid,
                    entry.new_mode.as_str(),
                    needle,
                    self.ignore_case,
                    self.ere.as_ref(),
                    blob_occurrence_cache.as_deref_mut(),
                )?;
                Ok(c1 != c2)
            }
            PickaxeKind::LineGrep => {
                let (old_bytes, old_bin) = blob_side(odb, entry.old_mode.as_str(), &entry.old_oid)?;
                let (new_bytes, new_bin) = blob_side(odb, entry.new_mode.as_str(), &entry.new_oid)?;
                if old_bin || new_bin {
                    return Ok(false);
                }
                let Some(regex) = self.ere.as_ref() else {
                    return Ok(false);
                };
                Ok(line_grep_match(
                    old_bytes.text().as_str(),
                    new_bytes.text().as_str(),
                    regex,
                ))
            }
        }
    }
}

fn blob_occurrence_count(
    odb: &crate::odb::Odb,
    oid: &ObjectId,
    mode: &str,
    needle: &str,
    ignore_case: bool,
    ere: Option<&PosixEre>,
    cache: Option<&mut HashMap<ObjectId, usize>>,
) -> Result<usize> {
    if mode == "000000" || *oid == ObjectId::zero() {
        return Ok(0);
    }
    if mode == "160000" {
        let (bytes, _) = blob_side(odb, mode, oid)?;
        return Ok(count_occurrences(
            bytes.data.as_slice(),
            needle,
            ignore_case,
            ere,
        ));
    }
    if let Some(cache) = cache {
        if ere.is_none() && !ignore_case {
            if let Some(&count) = cache.get(oid) {
                return Ok(count);
            }
            let data = odb.read(oid)?.data;
            let count = count_occurrences(&data, needle, false, None);
            cache.insert(*oid, count);
            return Ok(count);
        }
    }
    let (bytes, _) = blob_side(odb, mode, oid)?;
    Ok(count_occurrences(
        bytes.data.as_slice(),
        needle,
        ignore_case,
        ere,
    ))
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

/// Git `regcomp(..., REG_EXTENDED | REG_NEWLINE)` for pickaxe patterns.
fn compile_pickaxe_ere(
    pattern: &str,
    ignore_case: bool,
) -> std::result::Result<PosixEre, PickaxeError> {
    PosixEre::compile(pattern, ignore_case).map_err(|detail| PickaxeError::InvalidPattern {
        pattern: pattern.to_owned(),
        detail,
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

fn count_occurrences(
    haystack: &[u8],
    needle: &str,
    ignore_case: bool,
    ere: Option<&PosixEre>,
) -> usize {
    if haystack.is_empty() {
        return 0;
    }
    if let Some(re) = ere {
        return re.count_nonoverlapping(haystack);
    }
    if ignore_case {
        let h = String::from_utf8_lossy(haystack).to_lowercase();
        let n = needle.to_lowercase();
        return count_substr(h.as_bytes(), n.as_bytes());
    }
    count_substr(haystack, needle.as_bytes())
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

fn line_grep_match(old_text: &str, new_text: &str, regex: &PosixEre) -> bool {
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
