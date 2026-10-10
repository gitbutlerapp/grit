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
    pub ignore_all_space: bool,
    pub ignore_space_change: bool,
    pub ignore_space_at_eol: bool,
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
