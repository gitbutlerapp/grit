//! Case-insensitive path identity when `core.ignorecase` is enabled.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::index::{Index, IndexEntry, MODE_GITLINK, MODE_TREE};
use crate::index_name_hash_lazy::memihash;
use crate::objects::ObjectId;

type FoldBucket = Vec<Vec<u8>>;

fn push_bucket(map: &mut HashMap<u32, FoldBucket>, path: Vec<u8>) {
    map.entry(memihash(&path)).or_default().push(path);
}

fn bucket_has_path<P: AsRef<[u8]>>(bucket: &[P], path: &[u8]) -> bool {
    bucket
        .iter()
        .any(|candidate| candidate.as_ref().eq_ignore_ascii_case(path))
}

/// Stage-0 tracked paths for O(1) average lookups during untracked scans.
///
/// Borrows path bytes from the [`Index`] it was built from, so building the table does not
/// copy every indexed path.
#[derive(Debug)]
pub struct Stage0TrackedPaths<'a> {
    exact: hashbrown::HashSet<&'a [u8]>,
    folded: Option<HashMap<u32, Vec<&'a [u8]>>>,
}

impl<'a> Stage0TrackedPaths<'a> {
    /// Build a lookup table for untracked scanning: every indexed path counts as
    /// tracked, including unmerged conflict stages (1–3).
    #[must_use]
    pub fn from_index_for_untracked_scan(index: &'a Index, ignorecase: bool) -> Self {
        if ignorecase {
            let mut folded: HashMap<u32, Vec<&'a [u8]>> = HashMap::new();
            for e in &index.entries {
                folded
                    .entry(memihash(&e.path))
                    .or_default()
                    .push(e.path.as_slice());
            }
            Self {
                exact: hashbrown::HashSet::new(),
                folded: Some(folded),
            }
        } else {
            Self {
                exact: index.entries.iter().map(|e| e.path.as_slice()).collect(),
                folded: None,
            }
        }
    }

    /// Whether a repository-relative path is already tracked at stage 0 (raw index bytes).
    #[must_use]
    pub fn contains_bytes(&self, path: &[u8]) -> bool {
        if let Some(folded) = &self.folded {
            folded
                .get(&memihash(path))
                .is_some_and(|bucket| bucket_has_path(bucket, path))
        } else {
            self.exact.contains(path)
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

/// Whether two repository-relative paths can refer to the same on-disk file (Git `paths_collide`).
#[must_use]
pub fn paths_collide(a: &str, b: &str, ignorecase: bool) -> bool {
    if a.len() == b.len() {
        return paths_equal(a.as_bytes(), b.as_bytes(), ignorecase);
    }
    if a.len() < b.len() {
        return b.as_bytes().get(a.len()) == Some(&b'/')
            && paths_equal(a.as_bytes(), &b.as_bytes()[..a.len()], ignorecase);
    }
    a.as_bytes().get(b.len()) == Some(&b'/')
        && paths_equal(&a.as_bytes()[..b.len()], b.as_bytes(), ignorecase)
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

/// Stage-0 blob paths that share a worktree slot under `core.ignorecase` (sorted, unique).
#[must_use]
pub fn icase_checkout_collision_paths(index: &Index, ignorecase: bool) -> Vec<String> {
    if !ignorecase {
        return Vec::new();
    }
    let paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_TREE && e.mode != MODE_GITLINK)
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_owned))
        .collect();
    let groups = icase_collision_groups(&paths, ignorecase);
    let mut collided = BTreeSet::new();
    for group in groups {
        if group.len() > 1 {
            collided.extend(group);
        }
    }
    collided.into_iter().collect()
}

/// Count distinct worktree blob slots and total indexed bytes after an ignorecase checkout.
#[must_use]
pub fn icase_worktree_file_stats(index: &Index, ignorecase: bool) -> (u64, u64) {
    let mut files = 0u64;
    let mut bytes = 0u64;
    if !ignorecase {
        for e in &index.entries {
            if e.stage() == 0 && e.mode != MODE_TREE && e.mode != MODE_GITLINK {
                files += 1;
                bytes += u64::from(e.size);
            }
        }
        return (files, bytes);
    }
    let paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_TREE && e.mode != MODE_GITLINK)
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_owned))
        .collect();
    let groups = icase_collision_groups(&paths, ignorecase);
    for group in groups {
        files += 1;
        let Some(rep) = group.iter().min() else {
            continue;
        };
        if let Some(e) = index.get(rep.as_bytes(), 0) {
            bytes += u64::from(e.size);
        }
    }
    (files, bytes)
}

/// Whether another stage-0 path shares this index entry's worktree slot under `core.ignorecase`.
#[must_use]
pub fn has_icase_checkout_collision_sibling(index: &Index, index_rel: &str) -> bool {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_TREE && e.mode != MODE_GITLINK)
        .filter_map(|e| std::str::from_utf8(&e.path).ok())
        .any(|p| p != index_rel && paths_collide(p, index_rel, true))
}

/// When auto-staging tracked paths, skip index rows whose on-disk bytes match a colliding
/// sibling's indexed blob (the path that lost checkout on a case-insensitive filesystem).
#[must_use]
pub fn is_icase_collision_loser_for_auto_stage(
    index: &Index,
    entry: &IndexEntry,
    ignorecase: bool,
    worktree_blob_oid: ObjectId,
) -> bool {
    if !ignorecase || entry.stage() != 0 {
        return false;
    }
    let Ok(my_path) = std::str::from_utf8(&entry.path) else {
        return false;
    };
    if worktree_blob_oid == entry.oid {
        return false;
    }
    for other in &index.entries {
        if other.stage() != 0 || other.path == entry.path {
            continue;
        }
        let Ok(other_path) = std::str::from_utf8(&other.path) else {
            continue;
        };
        if paths_collide(my_path, other_path, true) && other.oid == worktree_blob_oid {
            return true;
        }
    }
    false
}

fn icase_collision_groups(paths: &[String], ignorecase: bool) -> Vec<Vec<String>> {
    let n = paths.len();
    if n == 0 {
        return Vec::new();
    }
    let mut parent: Vec<usize> = (0..n).collect();
    fn find(parent: &mut [usize], i: usize) -> usize {
        if parent[i] != i {
            let root = find(parent, parent[i]);
            parent[i] = root;
        }
        parent[i]
    }
    fn union(parent: &mut [usize], a: usize, b: usize) {
        let ra = find(parent, a);
        let rb = find(parent, b);
        if ra != rb {
            parent[rb] = ra;
        }
    }
    for i in 0..n {
        for j in (i + 1)..n {
            if paths_collide(&paths[i], &paths[j], ignorecase) {
                union(&mut parent, i, j);
            }
        }
    }
    let mut by_root: HashMap<usize, Vec<String>> = HashMap::new();
    for (i, path) in paths.iter().enumerate() {
        by_root
            .entry(find(&mut parent, i))
            .or_default()
            .push(path.clone());
    }
    let mut groups: Vec<Vec<String>> = by_root.into_values().collect();
    for g in &mut groups {
        g.sort();
    }
    groups.sort();
    groups
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
    fn paths_collide_dir_and_file() {
        assert!(paths_collide("a", "a/b", false));
        assert!(paths_collide("A/B", "a", true));
        assert!(!paths_collide("a/b", "a/c", false));
        assert!(paths_collide("File.txt", "file.txt", true));
        assert!(paths_collide("Dir/a.txt", "dir/A.txt", true));
    }

    #[test]
    fn icase_checkout_collision_paths_groups_case_pairs() {
        let mut index = Index::new();
        for path in ["Dir/a.txt", "File.txt", "dir/A.txt", "file.txt"] {
            index.push_entry_unsorted(dummy_entry(path));
        }
        index.sort();
        let collided = icase_checkout_collision_paths(&index, true);
        assert_eq!(
            collided,
            vec![
                "Dir/a.txt".to_owned(),
                "File.txt".to_owned(),
                "dir/A.txt".to_owned(),
                "file.txt".to_owned(),
            ]
        );
        let (files, _) = icase_worktree_file_stats(&index, true);
        assert_eq!(files, 2);
    }

    #[test]
    fn memihash_collision_requires_ascii_case_confirm() {
        assert_eq!(memihash(b"f02398b.txt"), memihash(b"f0688b8.txt"));
        let mut index = Index::new();
        index.push_entry_unsorted(dummy_entry("f02398b.txt"));
        let tracked = Stage0TrackedPaths::from_index_for_untracked_scan(&index, true);
        assert!(tracked.contains_bytes(b"f02398b.txt"));
        assert!(
            !tracked.contains_bytes(b"f0688b8.txt"),
            "hash collision must not treat distinct paths as tracked"
        );
    }
}
