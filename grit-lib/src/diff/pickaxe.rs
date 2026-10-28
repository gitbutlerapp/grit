//! Pickaxe filters (`-S` / `-G`) for revision walks and diffs.
//!
//! `-S` keeps commits where the number of occurrences of a string (or ERE pattern with
//! `--pickaxe-regex`) differs between a file's pre- and post-image compared to the first
//! parent. `-G` keeps commits where added or removed lines match a regex (Git line-level diff).
//! Merge commits are omitted unless combined merge diffs are requested (matches default `git log`).

use std::collections::HashMap;
use std::sync::Arc;

use crate::config::ConfigSet;
use crate::diff::core::diff_trees_for_pathspecs;
use crate::diff::posix_ere::PosixEre;
use crate::diff::structured::SideBytes;
use crate::diff::{
    detect_renames_with_policy, renames_from_config, DiffEntry, DiffStatus, RenameDetection,
};
use crate::error::{Error, Result};
use crate::objects::{parse_commit, FileMode, ObjectId};
use crate::parallel::ThreadPolicy;
use crate::repo::Repository;

/// Rename pairing a pickaxe applies to added and deleted paths before matching them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct PickaxeRenames<'a> {
    /// Similarity percent, or `None` when renames are not detected.
    threshold: Option<u32>,
    /// Source of `diff.renameLimit` and rename-scoring thread settings.
    config: &'a ConfigSet,
}

impl<'a> PickaxeRenames<'a> {
    pub(crate) fn new(detection: RenameDetection, config: &'a ConfigSet) -> Self {
        Self {
            threshold: detection.threshold_percent(),
            config,
        }
    }

    /// Git's `log` default: `diff.renames` from `config`, at the default similarity.
    pub(crate) fn from_config(config: &'a ConfigSet) -> Self {
        Self::new(renames_from_config(config), config)
    }

    pub(crate) fn config(&self) -> &'a ConfigSet {
        self.config
    }
}

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
    /// Matches Git's order of work: the tree diff is limited to `paths` first, then deleted and
    /// added paths are paired as renames per `renames`, and only then is each file pair tested.
    /// A rename whose old and new file hold the same number of matches (`-S`), or whose diff
    /// changes no matching line (`-G`), is therefore not a change. A file moved into the
    /// pathspec from outside it is an addition, since its old path was never diffed.
    ///
    /// Merge commits are skipped (Git default without combined merge diffs). Non-merge commits
    /// compare to the first parent only. Binary files are skipped for `-G`; `-S` searches binary
    /// blobs.
    ///
    /// # Parameters
    ///
    /// - `paths` — repository-relative pathspecs limiting the diff; empty means the whole tree.
    /// - `renames` — rename pairing; callers following Git use
    ///   [`renames_from_config`] (`diff.renames`).
    /// - `config` — supplies `diff.renameLimit` and rename-scoring thread settings.
    ///
    /// # Errors
    ///
    /// Propagates object read and diff errors.
    pub fn matches_commit(
        &self,
        repo: &Repository,
        commit_oid: ObjectId,
        paths: &[String],
        renames: RenameDetection,
        config: &ConfigSet,
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
        let renames = PickaxeRenames::new(renames, config);
        self.matches_trees(
            repo,
            parent_tree.as_ref(),
            &commit.tree,
            paths,
            renames,
            &mut HashMap::new(),
        )
    }

    /// Pickaxe match using pre-resolved parent and commit trees (avoids re-reading commits).
    ///
    /// `parent_tree` is `None` for root commits. Merge commits must be filtered out by the caller.
    /// `blob_occurrence_cache` memoizes literal `-S` counts per blob across calls.
    pub(crate) fn matches_trees(
        &self,
        repo: &Repository,
        parent_tree: Option<&ObjectId>,
        commit_tree: &ObjectId,
        paths: &[String],
        renames: PickaxeRenames<'_>,
        blob_occurrence_cache: &mut HashMap<ObjectId, usize>,
    ) -> Result<bool> {
        let diff_options = crate::diff::DiffOptions::from_config(renames.config());
        let odb = repo.odb();
        let entries = diff_trees_for_pathspecs(odb, parent_tree, Some(commit_tree), paths)?;
        // Rename pairing only ever combines an added path with a deleted one, so every other
        // entry can be tested (and can end the search) before paying for it.
        let mut unpaired = Vec::new();
        for entry in entries {
            if matches!(entry.status, DiffStatus::Added | DiffStatus::Deleted) {
                unpaired.push(entry);
            } else if self.changed_filepair_matches(
                odb,
                &entry,
                &diff_options,
                blob_occurrence_cache,
            )? {
                return Ok(true);
            }
        }
        if unpaired.is_empty() {
            return Ok(false);
        }
        // Any pair or lone side that matches has a side that matches on its own.
        let mut any_side_matches = false;
        for entry in &unpaired {
            if self.side_may_match(odb, entry, blob_occurrence_cache)? {
                any_side_matches = true;
                break;
            }
        }
        if !any_side_matches {
            return Ok(false);
        }
        let pairs = match renames.threshold {
            Some(threshold) => detect_renames_with_policy(
                odb,
                None,
                unpaired,
                threshold,
                renames.config,
                &ThreadPolicy::default(),
                None,
            )?,
            None => unpaired,
        };
        for entry in &pairs {
            if self.changed_filepair_matches(odb, entry, &diff_options, blob_occurrence_cache)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn changed_filepair_matches(
        &self,
        odb: &crate::odb::Odb,
        entry: &DiffEntry,
        diff_options: &crate::diff::DiffOptions,
        blob_occurrence_cache: &mut HashMap<ObjectId, usize>,
    ) -> Result<bool> {
        if entry.old_oid == entry.new_oid && entry.old_mode == entry.new_mode {
            return Ok(false);
        }
        self.filepair_matches(odb, entry, diff_options, blob_occurrence_cache)
    }

    /// Whether the one present side of an added or deleted `entry` holds a match by itself.
    fn side_may_match(
        &self,
        odb: &crate::odb::Odb,
        entry: &DiffEntry,
        blob_occurrence_cache: &mut HashMap<ObjectId, usize>,
    ) -> Result<bool> {
        let (oid, mode) = if entry.status == DiffStatus::Deleted {
            (&entry.old_oid, entry.old_mode)
        } else {
            (&entry.new_oid, entry.new_mode)
        };
        match &self.kind {
            PickaxeKind::Occurrences { .. } => {
                Ok(self.occurrences_in(odb, oid, mode, blob_occurrence_cache)? > 0)
            }
            PickaxeKind::LineGrep => {
                let (bytes, binary) = blob_side(odb, mode, oid)?;
                let Some(regex) = self.ere.as_ref().filter(|_| !binary) else {
                    return Ok(false);
                };
                Ok(regex.is_match(&bytes.data))
            }
        }
    }

    fn filepair_matches(
        &self,
        odb: &crate::odb::Odb,
        entry: &DiffEntry,
        diff_options: &crate::diff::DiffOptions,
        blob_occurrence_cache: &mut HashMap<ObjectId, usize>,
    ) -> Result<bool> {
        match &self.kind {
            PickaxeKind::Occurrences { .. } => {
                let old = self.occurrences_in(
                    odb,
                    &entry.old_oid,
                    entry.old_mode,
                    blob_occurrence_cache,
                )?;
                let new = self.occurrences_in(
                    odb,
                    &entry.new_oid,
                    entry.new_mode,
                    blob_occurrence_cache,
                )?;
                Ok(old != new)
            }
            PickaxeKind::LineGrep => {
                let (old_bytes, old_bin) = blob_side(odb, entry.old_mode, &entry.old_oid)?;
                let (new_bytes, new_bin) = blob_side(odb, entry.new_mode, &entry.new_oid)?;
                if old_bin || new_bin {
                    return Ok(false);
                }
                let Some(regex) = self.ere.as_ref() else {
                    return Ok(false);
                };
                // Under REG_NEWLINE a match within one line is also a match within its whole
                // blob, so a pair where neither side matches has no matching changed line.
                if !regex.is_match(&old_bytes.data) && !regex.is_match(&new_bytes.data) {
                    return Ok(false);
                }
                Ok(line_grep_match(
                    &String::from_utf8_lossy(&old_bytes.data),
                    &String::from_utf8_lossy(&new_bytes.data),
                    regex,
                    diff_options,
                ))
            }
        }
    }

    /// `-S` match count in one side of a file pair, memoized per blob in `cache`.
    ///
    /// The cache is only valid for this pickaxe: counts depend on the needle and its flags.
    fn occurrences_in(
        &self,
        odb: &crate::odb::Odb,
        oid: &ObjectId,
        mode: FileMode,
        cache: &mut HashMap<ObjectId, usize>,
    ) -> Result<usize> {
        let PickaxeKind::Occurrences { needle } = &self.kind else {
            return Ok(0);
        };
        if mode.is_absent() || *oid == ObjectId::zero() {
            return Ok(0);
        }
        let count_in =
            |bytes: &[u8]| count_occurrences(bytes, needle, self.ignore_case, self.ere.as_ref());
        if mode == FileMode::GITLINK {
            let (bytes, _) = blob_side(odb, mode, oid)?;
            return Ok(count_in(bytes.data.as_slice()));
        }
        if let Some(&count) = cache.get(oid) {
            return Ok(count);
        }
        let count = count_in(&odb.read(oid)?.data);
        cache.insert(*oid, count);
        Ok(count)
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
    memchr::memmem::find_iter(haystack, needle).count()
}

fn blob_side(odb: &crate::odb::Odb, mode: FileMode, oid: &ObjectId) -> Result<(SideBytes, bool)> {
    if mode == FileMode::GITLINK {
        let line = if *oid == ObjectId::zero() {
            String::new()
        } else {
            format!("Subproject commit {}\n", oid.to_hex())
        };
        return Ok((SideBytes::from_bytes(line.into_bytes()), false));
    }
    if mode.is_absent() || *oid == ObjectId::zero() {
        return Ok((SideBytes::from_bytes(Vec::new()), false));
    }
    let data = odb.read(oid)?.data;
    let binary = data.iter().take(8000).any(|&b| b == 0);
    Ok((SideBytes::from_bytes(data), binary))
}

fn line_grep_match(
    old_text: &str,
    new_text: &str,
    regex: &PosixEre,
    diff_options: &crate::diff::DiffOptions,
) -> bool {
    use crate::diff::core::compacted_line_ops_for_options;
    use crate::diff::unified_emit::LineSide;
    use similar::DiffOp;

    let before = LineSide::from_text(old_text);
    let after = LineSide::from_text(new_text);
    let old_lines: Vec<&str> = before.lines.iter().map(|l| l.body).collect();
    let new_lines: Vec<&str> = after.lines.iter().map(|l| l.body).collect();
    for op in compacted_line_ops_for_options(old_text, new_text, diff_options) {
        match op {
            DiffOp::Equal { .. } => {}
            DiffOp::Delete {
                old_index, old_len, ..
            } => {
                for line in &old_lines[old_index..old_index + old_len] {
                    if regex.is_match(line.as_bytes()) {
                        return true;
                    }
                }
            }
            DiffOp::Insert {
                new_index, new_len, ..
            } => {
                for line in &new_lines[new_index..new_index + new_len] {
                    if regex.is_match(line.as_bytes()) {
                        return true;
                    }
                }
            }
            DiffOp::Replace {
                old_index,
                old_len,
                new_index,
                new_len,
                ..
            } => {
                for line in &old_lines[old_index..old_index + old_len] {
                    if regex.is_match(line.as_bytes()) {
                        return true;
                    }
                }
                for line in &new_lines[new_index..new_index + new_len] {
                    if regex.is_match(line.as_bytes()) {
                        return true;
                    }
                }
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_counts_do_not_overlap() {
        assert_eq!(count_occurrences(b"aaaa", "aa", false, None), 2);
        assert_eq!(count_occurrences(b"aaa", "aa", false, None), 1);
        assert_eq!(count_occurrences(b"abcabc", "abc", false, None), 2);
        assert_eq!(count_occurrences(b"abc", "", false, None), 0);
        assert_eq!(count_occurrences(b"", "a", false, None), 0);
    }

    #[test]
    fn ignore_case_counts_fold_both_sides() {
        assert_eq!(count_occurrences(b"Needle NEEDLE", "needle", true, None), 2);
        assert_eq!(
            count_occurrences(b"Needle NEEDLE", "needle", false, None),
            0
        );
    }
}
