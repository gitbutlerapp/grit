//! Case-insensitive path identity when `core.ignorecase` is enabled.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::index::Index;
use crate::index_name_hash_lazy::memihash;

type FoldBucket = Vec<Vec<u8>>;

fn push_bucket(map: &mut HashMap<u32, FoldBucket>, path: Vec<u8>) {
    map.entry(memihash(&path)).or_default().push(path);
}

fn bucket_has_path(bucket: &[Vec<u8>], path: &[u8]) -> bool {
    bucket
        .iter()
        .any(|candidate| candidate.eq_ignore_ascii_case(path))
}

/// Stage-0 tracked paths for O(1) average lookups during untracked scans.
#[derive(Debug)]
pub struct Stage0TrackedPaths {
    exact: HashSet<Vec<u8>>,
    folded: Option<HashMap<u32, FoldBucket>>,
}

impl Stage0TrackedPaths {
    /// Build a lookup table from the index once per worktree walk.
    #[must_use]
    pub fn from_index(index: &Index, ignorecase: bool) -> Self {
        if ignorecase {
            let mut folded = HashMap::new();
            for e in &index.entries {
                if e.stage() == 0 {
                    push_bucket(&mut folded, e.path.clone());
                }
            }
            Self {
                exact: HashSet::new(),
                folded: Some(folded),
            }
        } else {
            let mut exact = HashSet::with_capacity(index.entries.len());
            for e in &index.entries {
                if e.stage() == 0 {
                    exact.insert(e.path.clone());
                }
            }
            Self {
                exact,
                folded: None,
            }
        }
    }

    /// Whether a repository-relative path is already tracked at stage 0.
    #[must_use]
    pub fn contains(&self, path: &str) -> bool {
        let bytes = path.as_bytes();
        if let Some(folded) = &self.folded {
            folded
                .get(&memihash(bytes))
                .is_some_and(|bucket| bucket_has_path(bucket, bytes))
        } else {
            self.exact.contains(bytes)
        }
    }
}

/// Map from case-folded path hash buckets to index spellings (built once per staging batch).
#[derive(Debug, Default)]
pub struct Stage0IcasePathMap {
    by_fold: HashMap<u32, FoldBucket>,
}

impl Stage0IcasePathMap {
    /// Snapshot stage-0 paths for case-insensitive replacement during `git add`.
    #[must_use]
    pub fn from_index(index: &Index, ignorecase: bool) -> Self {
        if !ignorecase {
            return Self::default();
        }
        let mut by_fold = HashMap::new();
        for e in &index.entries {
            if e.stage() == 0 {
                push_bucket(&mut by_fold, e.path.clone());
            }
        }
        Self { by_fold }
    }

    /// Remove the index entry that aliases `path` on a case-insensitive filesystem.
    pub fn remove_alias_of(&self, index: &mut Index, path: &[u8]) -> bool {
        let Some(bucket) = self.by_fold.get(&memihash(path)) else {
            return false;
        };
        let mut removed = false;
        for index_path in bucket {
            if index_path.eq_ignore_ascii_case(path) && index.remove(index_path) {
                removed = true;
            }
        }
        removed
    }
}

/// Compare two repository-relative paths for identity.
#[must_use]
pub fn paths_equal(path_a: &[u8], path_b: &[u8], ignorecase: bool) -> bool {
    if path_a == path_b {
        return true;
    }
    ignorecase && path_a.eq_ignore_ascii_case(path_b)
}

/// Resolve a worktree path for an index entry, honoring case-insensitive filesystems.
#[must_use]
pub fn worktree_path_for_index_entry(
    work_tree: &Path,
    index_rel: &str,
    ignorecase: bool,
) -> PathBuf {
    let direct = work_tree.join(index_rel);
    if !ignorecase || direct.exists() {
        return direct;
    }
    resolve_case_alternate(work_tree, index_rel).unwrap_or(direct)
}

fn resolve_case_alternate(work_tree: &Path, rel: &str) -> Option<PathBuf> {
    let rel_path = Path::new(rel);
    let file_name = rel_path.file_name()?;
    let parent_abs = match rel_path.parent() {
        None => work_tree.to_path_buf(),
        Some(p) if p.as_os_str().is_empty() => work_tree.to_path_buf(),
        Some(p) => work_tree.join(p),
    };
    let want = file_name.to_string_lossy();
    let entries = std::fs::read_dir(&parent_abs).ok()?;
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .eq_ignore_ascii_case(want.as_ref())
        {
            return Some(entry.path());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;
    use crate::objects::ObjectId;

    fn dummy_entry(path: &str) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid: ObjectId::from_bytes(&[0u8; 20]).unwrap(),
            flags: 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn memihash_collision_requires_ascii_case_confirm() {
        assert_eq!(memihash(b"f02398b.txt"), memihash(b"f0688b8.txt"));
        let mut index = Index::new();
        index.push_entry_unsorted(dummy_entry("f02398b.txt"));
        let tracked = Stage0TrackedPaths::from_index(&index, true);
        assert!(tracked.contains("f02398b.txt"));
        assert!(
            !tracked.contains("f0688b8.txt"),
            "hash collision must not treat distinct paths as tracked"
        );
    }
}
