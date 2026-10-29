//! Merge-strategy helpers shared by pick/revert replay and other tree merges.
//!
//! End-to-end single-commit replay (three-way merge, checkout, commit, and ref
//! update) lives in [`crate::porcelain::replay`]. What lives here is directory-rename
//! detection over index data: exact (same-blob) rename pairing and
//! [`directory_renames_from_file_renames`], which aggregates file renames into
//! directory renames.

use std::collections::HashMap;

use crate::index::IndexEntry;
use crate::repo_relpath::parent_dir;

/// Detect exact (same-blob) file renames between `base` and `side`: a path present only in
/// `base` paired with a path present only in `side` whose entry has identical oid and mode.
///
/// Works over ordered sparse merge maps without copying entries. The first added path for
/// each blob/mode wins, and several sources may pair with it.
pub(crate) fn same_blob_renames_from_entries<'a>(
    base: impl IntoIterator<Item = (&'a Vec<u8>, &'a IndexEntry)>,
    side: impl IntoIterator<Item = (&'a Vec<u8>, &'a IndexEntry)>,
    base_contains: impl Fn(&[u8]) -> bool,
    side_contains: impl Fn(&[u8]) -> bool,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut added = HashMap::new();
    for (path, entry) in side {
        if !base_contains(path) {
            added.entry((entry.oid, entry.mode)).or_insert(path);
        }
    }
    base.into_iter()
        .filter(|(path, _)| !side_contains(path))
        .filter_map(|(path, entry)| {
            added
                .get(&(entry.oid, entry.mode))
                .map(|new| (path.clone(), (*new).clone()))
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::zero_oid;
    use crate::index::MODE_REGULAR;

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

    fn same_blob_renames(
        base: &HashMap<Vec<u8>, IndexEntry>,
        side: &HashMap<Vec<u8>, IndexEntry>,
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        same_blob_renames_from_entries(
            base.iter(),
            side.iter(),
            |path| base.contains_key(path),
            |path| side.contains_key(path),
        )
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
    fn same_blob_renames_keeps_first_match_and_reuses_destinations() {
        let base: HashMap<_, _> = ["old-a", "old-b", "unchanged", "mode-change"]
            .into_iter()
            .map(|path| (path.as_bytes().to_vec(), entry(path)))
            .collect();
        let mut side: HashMap<_, _> = ["new-a", "new-b", "unchanged", "mode-change"]
            .into_iter()
            .map(|path| (path.as_bytes().to_vec(), entry(path)))
            .collect();
        side.get_mut(b"mode-change".as_slice()).expect("entry").mode = 0o100755;
        let added: Vec<_> = side
            .iter()
            .filter(|(path, _)| !base.contains_key(*path))
            .collect();
        let expected: Vec<_> = base
            .iter()
            .filter(|(path, _)| !side.contains_key(*path))
            .filter_map(|(old, entry)| {
                added
                    .iter()
                    .find(|(_, candidate)| {
                        entry.oid == candidate.oid && entry.mode == candidate.mode
                    })
                    .map(|(new, _)| (old.clone(), (*new).clone()))
            })
            .collect();
        let actual = same_blob_renames(&base, &side);
        assert_eq!(actual, expected);
        assert_eq!(actual.len(), 2);
        assert_eq!(actual[0].1, actual[1].1);
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
    fn directory_renames_tie_drops_ambiguous_mapping() {
        let renames = vec![
            (b"a/x".to_vec(), b"b/x".to_vec()),
            (b"a/y".to_vec(), b"c/y".to_vec()),
        ];
        let dirs = directory_renames_from_file_renames(&renames);
        assert!(!dirs.contains_key(b"a".as_slice()));
    }
}
