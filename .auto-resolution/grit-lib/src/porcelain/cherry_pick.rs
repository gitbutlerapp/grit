//! Merge-strategy helpers shared by pick/revert replay and other tree merges.
//!
//! End-to-end single-commit replay (three-way merge, checkout, commit, and ref
//! update) lives in [`crate::porcelain::replay`]. What lives here are the pure
//! data transforms over index/tree data: strategy-option parsing, directory-rename
//! detection, and conflict-stage staging helpers.
//!
//! # What this module owns
//!
//! - [`parse_strategy_options`] / [`WhitespaceStrategyOptions`] — translate
//!   `-X<option>` merge-strategy options into a [`MergeFavor`], whitespace flags,
//!   and an optional diff algorithm.
//! - The directory-rename detection cluster
//!   ([`same_blob_renames`], [`directory_renames_from_file_renames`],
//!   [`remap_path_by_directory_renames`]) and the index-staging helpers
//!   ([`stage_entry_at`], [`path_has_unmerged_entry`], [`same_blob`]) the CLI
//!   uses to surface Git's transitive "file location" conflicts.

use std::collections::HashMap;

use crate::index::{Index, IndexEntry};
use crate::merge_file::MergeFavor;

/// Whitespace-handling flags parsed from `-X<option>` merge-strategy options.
#[derive(Clone, Copy, Debug, Default)]
pub struct WhitespaceStrategyOptions {
    /// Ignore all whitespace when comparing lines (`-Xignore-all-space`).
    pub ignore_all_space: bool,
    /// Ignore whitespace runs when comparing lines (`-Xignore-space-change`).
    pub ignore_space_change: bool,
    /// Ignore whitespace at end of line (`-Xignore-space-at-eol`).
    pub ignore_space_at_eol: bool,
    /// Ignore CR at end of line on the old side (`-Xignore-cr-at-eol`).
    pub ignore_cr_at_eol: bool,
}

/// Parse `-X<option>` strategy options into a merge favor, whitespace options, and
/// an optional diff algorithm.
///
/// Recognises Git's `-Xtheirs`/`-Xours` (resolution favor), the `ignore-*-space`
/// whitespace flags, and `diff-algorithm=<algo>` / bare `histogram`|`patience`
/// (diff algorithm). Unrecognised options are ignored.
#[must_use]
pub fn parse_strategy_options(
    strategy_options: &[String],
) -> (MergeFavor, WhitespaceStrategyOptions, Option<String>) {
    let mut favor = MergeFavor::None;
    let mut ws = WhitespaceStrategyOptions::default();
    let mut diff_algorithm = None;
    for opt in strategy_options {
        if let Some(algo) = opt.strip_prefix("diff-algorithm=") {
            diff_algorithm = Some(algo.to_string());
            continue;
        }
        match opt.as_str() {
            "histogram" | "patience" => diff_algorithm = Some(opt.to_string()),
            "theirs" => favor = MergeFavor::Theirs,
            "ours" => favor = MergeFavor::Ours,
            "ignore-all-space" => ws.ignore_all_space = true,
            "ignore-space-change" => ws.ignore_space_change = true,
            "ignore-space-at-eol" => ws.ignore_space_at_eol = true,
            "ignore-cr-at-eol" => ws.ignore_cr_at_eol = true,
            _ => {}
        }
    }
    (favor, ws, diff_algorithm)
}

/// Whether two index entries name the same blob (identical oid and mode).
#[must_use]
pub fn same_blob(a: &IndexEntry, b: &IndexEntry) -> bool {
    a.oid == b.oid && a.mode == b.mode
}

/// The parent directory portion of a repository path (`a/b/c` -> `a/b`; top-level -> empty).
#[must_use]
pub fn parent_dir(path: &[u8]) -> Vec<u8> {
    path.iter()
        .rposition(|b| *b == b'/')
        .map_or_else(Vec::new, |pos| path[..pos].to_vec())
}

/// Remap `path` through a set of directory renames, choosing the longest matching source
/// directory. Returns `None` when no rename applies.
#[must_use]
pub fn remap_path_by_directory_renames(
    path: &[u8],
    dir_renames: &HashMap<Vec<u8>, Vec<u8>>,
) -> Option<Vec<u8>> {
    let mut best: Option<(&Vec<u8>, &Vec<u8>)> = None;
    for (old_dir, new_dir) in dir_renames {
        let matches = if old_dir.is_empty() {
            !path.contains(&b'/')
        } else {
            path.len() > old_dir.len()
                && path.starts_with(old_dir)
                && path.get(old_dir.len()) == Some(&b'/')
        };
        if !matches {
            continue;
        }
        if best.is_none_or(|(best_old, _)| old_dir.len() > best_old.len()) {
            best = Some((old_dir, new_dir));
        }
    }

    let (old_dir, new_dir) = best?;
    let suffix = if old_dir.is_empty() {
        path
    } else {
        &path[old_dir.len() + 1..]
    };
    let mut remapped = new_dir.clone();
    if !remapped.is_empty() && !suffix.is_empty() {
        remapped.push(b'/');
    }
    remapped.extend_from_slice(suffix);
    Some(remapped)
}

/// Detect exact (same-blob) file renames between `base` and `side`: a path present only in
/// `base` paired with a path present only in `side` whose entry has identical oid and mode.
#[must_use]
pub fn same_blob_renames(
    base: &HashMap<Vec<u8>, IndexEntry>,
    side: &HashMap<Vec<u8>, IndexEntry>,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let added: Vec<(&Vec<u8>, &IndexEntry)> = side
        .iter()
        .filter(|(path, _)| !base.contains_key(*path))
        .collect();
    let mut renames = Vec::new();
    for (old_path, base_entry) in base.iter().filter(|(path, _)| !side.contains_key(*path)) {
        if let Some((new_path, _)) = added
            .iter()
            .find(|(_, side_entry)| same_blob(base_entry, side_entry))
        {
            renames.push((old_path.clone(), (*new_path).clone()));
        }
    }
    renames
}

/// Aggregate file renames into directory renames: for each source directory, the destination
/// directory chosen by the most file renames (ties drop the directory rename entirely).
#[must_use]
pub fn directory_renames_from_file_renames(
    renames: &[(Vec<u8>, Vec<u8>)],
) -> HashMap<Vec<u8>, Vec<u8>> {
    let mut counts: HashMap<Vec<u8>, HashMap<Vec<u8>, usize>> = HashMap::new();
    for (old_path, new_path) in renames {
        let old_dir = parent_dir(old_path);
        let new_dir = parent_dir(new_path);
        if old_dir == new_dir {
            continue;
        }
        *counts
            .entry(old_dir)
            .or_default()
            .entry(new_dir)
            .or_default() += 1;
    }

    let mut dir_renames = HashMap::new();
    for (old_dir, destinations) in counts {
        let mut best: Option<(Vec<u8>, usize)> = None;
        let mut tied = false;
        for (new_dir, count) in destinations {
            match best {
                None => {
                    best = Some((new_dir, count));
                    tied = false;
                }
                Some((_, best_count)) if count > best_count => {
                    best = Some((new_dir, count));
                    tied = false;
                }
                Some((_, best_count)) if count == best_count => {
                    tied = true;
                }
                _ => {}
            }
        }
        if !tied {
            if let Some((new_dir, _)) = best {
                dir_renames.insert(old_dir, new_dir);
            }
        }
    }
    dir_renames
}

/// Push a copy of `src` into `index` at `path` with the given conflict `stage`.
pub fn stage_entry_at(index: &mut Index, path: &[u8], src: &IndexEntry, stage: u8) {
    let mut entry = src.clone();
    entry.path = path.to_vec();
    entry.flags = (path.len().min(0x0FFF) as u16) | ((stage as u16) << 12);
    index.push_entry_unsorted(entry);
}

/// Whether `index` already holds an unmerged (non-stage-0) entry at `path`.
#[must_use]
pub fn path_has_unmerged_entry(index: &Index, path: &[u8]) -> bool {
    index
        .entries
        .iter()
        .any(|entry| entry.path == path && entry.stage() != 0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::zero_oid;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};

    fn entry(path: &str) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 0,
            oid: zero_oid(),
            flags: 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn same_blob_renames_pairs_deleted_and_added_paths() {
        let mut base = HashMap::new();
        base.insert(b"old.txt".to_vec(), entry("old.txt"));
        let mut side = HashMap::new();
        side.insert(b"new.txt".to_vec(), entry("new.txt"));
        let renames = same_blob_renames(&base, &side);
        assert_eq!(renames.len(), 1);
        assert_eq!(renames[0].0, b"old.txt");
        assert_eq!(renames[0].1, b"new.txt");
    }

    #[test]
    fn remap_path_by_directory_renames_longest_prefix() {
        let mut dirs = HashMap::new();
        dirs.insert(b"old".to_vec(), b"new".to_vec());
        let remapped = remap_path_by_directory_renames(b"old/deep/file.txt", &dirs);
        assert_eq!(remapped.as_deref(), Some(b"new/deep/file.txt".as_slice()));
    }

    #[test]
    fn stage_entry_at_and_unmerged_detection() {
        let mut index = Index::new();
        let e = entry("x.txt");
        stage_entry_at(&mut index, b"x.txt", &e, 2);
        assert!(path_has_unmerged_entry(&index, b"x.txt"));
    }

    #[test]
    fn directory_renames_from_file_renames_picks_majority() {
        let renames = vec![
            (b"a/x".to_vec(), b"b/x".to_vec()),
            (b"a/y".to_vec(), b"b/y".to_vec()),
        ];
        let dirs = directory_renames_from_file_renames(&renames);
        assert_eq!(
            dirs.get(b"a".as_slice()).map(|v| v.as_slice()),
            Some(b"b".as_slice())
        );
    }

    #[test]
    fn parse_strategy_options_recognizes_favor_whitespace_and_diff_algo() {
        let opts = vec![
            "ours".into(),
            "ignore-all-space".into(),
            "diff-algorithm=histogram".into(),
            "patience".into(),
        ];
        let (favor, ws, algo) = parse_strategy_options(&opts);
        assert_eq!(favor, MergeFavor::Ours);
        assert!(ws.ignore_all_space);
        assert_eq!(algo.as_deref(), Some("patience"));
    }

    #[test]
    fn parent_dir_and_root_level_directory_remap() {
        assert_eq!(parent_dir(b"a/b/c"), b"a/b");
        let mut dirs = HashMap::new();
        dirs.insert(Vec::new(), b"top".to_vec());
        let remapped = remap_path_by_directory_renames(b"file.txt", &dirs);
        assert_eq!(remapped.as_deref(), Some(b"top/file.txt".as_slice()));
    }

    #[test]
    fn directory_renames_tie_drops_ambiguous_mapping() {
        let renames = vec![
            (b"a/x".to_vec(), b"b/x".to_vec()),
            (b"a/y".to_vec(), b"c/y".to_vec()),
        ];
        let dirs = directory_renames_from_file_renames(&renames);
        assert!(!dirs.contains_key(b"a".as_slice()));
    }
}
