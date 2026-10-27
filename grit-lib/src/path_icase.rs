//! Case-insensitive path identity when `core.ignorecase` is enabled.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use crate::index::{Index, MODE_GITLINK, MODE_TREE};
use crate::index_name_hash_lazy::memihash;

type FoldBucket = Vec<Vec<u8>>;

fn push_bucket(map: &mut HashMap<u32, FoldBucket>, path: Vec<u8>) {
    map.entry(memihash(&path)).or_default().push(path);
}

/// Add every leading directory of `path` (`a`, `a/b` for `a/b/c`), deepest first, stopping
/// at the first one already present since its ancestors were added with it.
fn insert_leading_dirs<'a>(dirs: &mut hashbrown::HashSet<&'a [u8]>, path: &'a [u8]) {
    let mut end = path.len();
    while let Some(slash) = path[..end].iter().rposition(|&b| b == b'/') {
        if !dirs.insert(&path[..slash]) {
            return;
        }
        end = slash;
    }
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
    /// Leading directories of stage-0 entries, bucketed by case-folded hash (`ignorecase` only).
    folded_dirs: Option<HashMap<u32, Vec<&'a [u8]>>>,
}

impl<'a> Stage0TrackedPaths<'a> {
    /// Build a lookup table for untracked scanning: every indexed path counts as
    /// tracked, including unmerged conflict stages (1–3).
    #[must_use]
    pub fn from_index_for_untracked_scan(index: &'a Index, ignorecase: bool) -> Self {
        if ignorecase {
            let mut folded: HashMap<u32, Vec<&'a [u8]>> = HashMap::new();
            let mut dirs: hashbrown::HashSet<&'a [u8]> = hashbrown::HashSet::new();
            for e in &index.entries {
                folded
                    .entry(memihash(&e.path))
                    .or_default()
                    .push(e.path.as_slice());
                if e.stage() == 0 {
                    insert_leading_dirs(&mut dirs, &e.path);
                }
            }
            let mut folded_dirs: HashMap<u32, Vec<&'a [u8]>> = HashMap::new();
            for dir in dirs {
                folded_dirs.entry(memihash(dir)).or_default().push(dir);
            }
            Self {
                exact: hashbrown::HashSet::new(),
                folded: Some(folded),
                folded_dirs: Some(folded_dirs),
            }
        } else {
            Self {
                exact: index.entries.iter().map(|e| e.path.as_slice()).collect(),
                folded: None,
                folded_dirs: None,
            }
        }
    }

    /// Whether some stage-0 entry lives under directory `dir` spelled with different ASCII
    /// case (Git's case-insensitive directory lookup under `core.ignorecase`).
    ///
    /// Always `false` for a table built without `ignorecase`; exact-case descendants are
    /// answered by [`Index::has_stage0_descendant_under`].
    #[must_use]
    pub fn has_dir_ignoring_case(&self, dir: &[u8]) -> bool {
        self.folded_dirs.as_ref().is_some_and(|dirs| {
            dirs.get(&memihash(dir))
                .is_some_and(|bucket| bucket_has_path(bucket, dir))
        })
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

type ReadDirFn<'r> = dyn FnMut(&Path) -> std::io::Result<std::fs::ReadDir> + 'r;

/// On-disk entry names of one directory, keyed by ASCII-lowercased name; each bucket is
/// sorted lexicographically.
type DentryListing = HashMap<String, Vec<String>>;

/// Directory listings memoized for one pass over the work tree.
///
/// Resolving the on-disk spelling of a path needs each parent's entries; reading them once per
/// directory keeps a pass over `n` paths linear instead of `n × files-per-directory`. The cache
/// does not observe later work tree changes, so build a fresh one for every pass.
#[derive(Debug, Default)]
pub(crate) struct DentrySpellingCache {
    /// `None` when the directory could not be read (missing, not a directory).
    dirs: HashMap<PathBuf, Option<DentryListing>>,
}

impl DentrySpellingCache {
    /// Repository-relative path using each parent directory entry's on-disk spelling, as
    /// [`worktree_dentry_spelling`] does, reading every directory at most once per cache.
    pub(crate) fn spelling(&mut self, work_tree: &Path, index_rel: &str) -> Option<String> {
        self.spelling_with(work_tree, index_rel, &mut |dir: &Path| {
            std::fs::read_dir(dir)
        })
    }

    fn spelling_with(
        &mut self,
        work_tree: &Path,
        index_rel: &str,
        read_dir: &mut ReadDirFn<'_>,
    ) -> Option<String> {
        let components: Vec<&str> = index_rel.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() {
            return None;
        }
        self.spelling_from(work_tree, &components, read_dir)
            .filter(|s| !s.is_empty())
    }

    /// Entries of `parent_abs` matching `want` ignoring ASCII case: the exact spelling first,
    /// then the rest lexicographically.
    fn candidates(
        &mut self,
        parent_abs: &Path,
        want: &str,
        read_dir: &mut ReadDirFn<'_>,
    ) -> Vec<String> {
        let listing = self
            .dirs
            .entry(parent_abs.to_path_buf())
            .or_insert_with(|| read_listing(parent_abs, read_dir));
        let Some(names) = listing
            .as_ref()
            .and_then(|l| l.get(&want.to_ascii_lowercase()))
        else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(names.len());
        out.extend(names.iter().filter(|n| n.as_str() == want).cloned());
        out.extend(names.iter().filter(|n| n.as_str() != want).cloned());
        out
    }

    fn spelling_from(
        &mut self,
        parent_abs: &Path,
        components: &[&str],
        read_dir: &mut ReadDirFn<'_>,
    ) -> Option<String> {
        let [head, tail @ ..] = components else {
            return Some(String::new());
        };
        for actual in self.candidates(parent_abs, head, read_dir) {
            let child_abs = parent_abs.join(&actual);
            let Some(suffix) = self.spelling_from(&child_abs, tail, read_dir) else {
                continue;
            };
            return Some(if suffix.is_empty() {
                actual
            } else {
                format!("{actual}/{suffix}")
            });
        }
        None
    }
}

fn read_listing(parent_abs: &Path, read_dir: &mut ReadDirFn<'_>) -> Option<DentryListing> {
    let entries = read_dir(parent_abs).ok()?;
    let mut listing = DentryListing::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        listing
            .entry(name.to_ascii_lowercase())
            .or_default()
            .push(name);
    }
    for names in listing.values_mut() {
        names.sort_unstable();
    }
    Some(listing)
}

/// Repository-relative path using each parent directory entry's on-disk spelling.
///
/// On case-insensitive volumes this reflects the dentry spelling from `readdir`, not the
/// index's lexical path (Git's `core.ignorecase` behavior). Each call reads every parent
/// directory; passes over many paths use a shared listing cache internally instead.
///
/// Returns `None` when no on-disk entry matches some component of `index_rel`.
#[must_use]
pub fn worktree_dentry_spelling(work_tree: &Path, index_rel: &str) -> Option<String> {
    DentrySpellingCache::default().spelling(work_tree, index_rel)
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
    if let Some(spelling) = worktree_dentry_spelling(work_tree, index_rel) {
        return work_tree.join(spelling);
    }
    direct
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

/// Stage-0 checkout collision groups keyed by a stable group id (built once per pass).
#[derive(Debug, Default)]
pub(crate) struct IcaseIndexCollisionGroups {
    path_to_group: HashMap<Vec<u8>, usize>,
    members_by_group: HashMap<usize, Vec<String>>,
}

impl IcaseIndexCollisionGroups {
    pub(crate) fn from_index(index: &Index, ignorecase: bool) -> Self {
        if !ignorecase {
            return Self::default();
        }
        let paths = stage0_blob_paths(index);
        let grouped = icase_collision_groups(&paths, true);
        let mut path_to_group = HashMap::new();
        let mut members_by_group = HashMap::new();
        for (key, group) in grouped.into_iter().enumerate() {
            for path in &group {
                path_to_group.insert(path.clone().into_bytes(), key);
            }
            members_by_group.insert(key, group);
        }
        Self {
            path_to_group,
            members_by_group,
        }
    }

    pub(crate) fn has_collision_sibling(&self, index_rel: &str) -> bool {
        self.path_to_group
            .get(index_rel.as_bytes())
            .and_then(|key| self.members_by_group.get(key))
            .is_some_and(|m| m.len() > 1)
    }
}

/// Ignorecase checkout collision map for auto-stage: group membership and dentry winner.
#[derive(Debug, Default)]
pub(crate) struct IcaseCheckoutCollisionMap {
    groups: IcaseIndexCollisionGroups,
    winner_by_group: HashMap<usize, String>,
}

impl IcaseCheckoutCollisionMap {
    pub(crate) fn from_index(work_tree: &Path, index: &Index, ignorecase: bool) -> Self {
        let groups = IcaseIndexCollisionGroups::from_index(index, ignorecase);
        if !ignorecase {
            return Self {
                groups,
                winner_by_group: HashMap::new(),
            };
        }
        let mut winner_by_group = HashMap::new();
        let mut dentries = DentrySpellingCache::default();
        for (key, members) in &groups.members_by_group {
            if members.len() <= 1 {
                continue;
            }
            let winner = resolve_collision_group_dentry_winner(work_tree, members, &mut dentries);
            winner_by_group.insert(*key, winner);
        }
        Self {
            groups,
            winner_by_group,
        }
    }

    /// Skip auto-stage refresh for collision aliases; the dentry winner is never a loser.
    pub(crate) fn is_auto_stage_loser(&self, raw_path: &[u8]) -> bool {
        let Some(key) = self.groups.path_to_group.get(raw_path) else {
            return false;
        };
        let Some(members) = self.groups.members_by_group.get(key) else {
            return false;
        };
        if members.len() <= 1 {
            return false;
        }
        let Some(winner) = self.winner_by_group.get(key) else {
            return false;
        };
        raw_path != winner.as_bytes()
    }

    /// Losers stay in the index when the worktree file is missing; winners may be removed.
    pub(crate) fn preserve_missing_collision_loser(&self, raw_path: &[u8]) -> bool {
        self.is_auto_stage_loser(raw_path)
    }
}

fn stage0_blob_paths(index: &Index) -> Vec<String> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_TREE && e.mode != MODE_GITLINK)
        .filter_map(|e| std::str::from_utf8(&e.path).ok().map(str::to_owned))
        .collect()
}

fn resolve_collision_group_dentry_winner(
    work_tree: &Path,
    members: &[String],
    dentries: &mut DentrySpellingCache,
) -> String {
    if let Some(dentry) = members.iter().find_map(|p| dentries.spelling(work_tree, p)) {
        if let Some(exact) = members.iter().find(|m| **m == dentry) {
            return exact.clone();
        }
    }
    icase_group_fallback_winner(members)
}

fn icase_group_fallback_winner(members: &[String]) -> String {
    let fold = members[0].to_ascii_lowercase();
    if let Some(exact) = members.iter().find(|m| m.as_str() == fold) {
        return exact.clone();
    }
    let mut candidates: Vec<&String> = members
        .iter()
        .filter(|m| m.to_ascii_lowercase() == fold)
        .collect();
    candidates.sort_by(|a, b| compare_icase_spelling_preference(a, b));
    candidates
        .first()
        .copied()
        .cloned()
        .unwrap_or_else(|| members[0].clone())
}

fn uppercase_component_count(path: &str) -> usize {
    path.chars().filter(|c| c.is_ascii_uppercase()).count()
}

fn compare_icase_spelling_preference(a: &str, b: &str) -> std::cmp::Ordering {
    uppercase_component_count(a)
        .cmp(&uppercase_component_count(b))
        .then_with(|| {
            let a_parts: Vec<&str> = a.split('/').collect();
            let b_parts: Vec<&str> = b.split('/').collect();
            for (ap, bp) in a_parts.iter().zip(b_parts.iter()) {
                let ao = uppercase_component_count(ap);
                let bo = uppercase_component_count(bp);
                if ao != bo {
                    return ao.cmp(&bo);
                }
            }
            std::cmp::Ordering::Equal
        })
        .then_with(|| a.cmp(b))
}

/// Count distinct worktree blob slots and total indexed bytes after an ignorecase checkout.
#[must_use]
pub fn icase_worktree_file_stats(
    index: &Index,
    work_tree: Option<&Path>,
    ignorecase: bool,
) -> (u64, u64) {
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
    let paths = stage0_blob_paths(index);
    let groups = icase_collision_groups(&paths, ignorecase);
    let mut dentries = DentrySpellingCache::default();
    for group in groups {
        files += 1;
        let mut group_bytes = 0u64;
        if let Some(wt) = work_tree {
            let probe = if group.len() > 1 {
                resolve_collision_group_dentry_winner(wt, &group, &mut dentries)
            } else {
                group[0].clone()
            };
            let abs = worktree_path_for_index_entry(wt, &probe, true);
            if abs.is_file() {
                if let Ok(meta) = std::fs::symlink_metadata(&abs) {
                    group_bytes = meta.len();
                }
            }
        }
        if group_bytes == 0 {
            for path in &group {
                if let Some(e) = index.get(path.as_bytes(), 0) {
                    group_bytes = group_bytes.max(u64::from(e.size));
                }
            }
        }
        bytes += group_bytes;
    }
    (files, bytes)
}

fn icase_collision_groups(paths: &[String], ignorecase: bool) -> Vec<Vec<String>> {
    let n = paths.len();
    if n == 0 {
        return Vec::new();
    }
    if !ignorecase {
        return paths.iter().map(|p| vec![p.clone()]).collect();
    }
    let normalized: Vec<Vec<u8>> = paths
        .iter()
        .map(|p| p.to_ascii_lowercase().into_bytes())
        .collect();
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

    let mut by_full: HashMap<u32, Vec<usize>> = HashMap::new();
    for (i, key) in normalized.iter().enumerate() {
        by_full.entry(memihash(key)).or_default().push(i);
    }
    for indices in by_full.values() {
        for a in 0..indices.len() {
            for b in (a + 1)..indices.len() {
                let i = indices[a];
                let j = indices[b];
                if paths_collide(&paths[i], &paths[j], true) {
                    union(&mut parent, i, j);
                }
            }
        }
    }

    let full_path_set: std::collections::HashSet<u32> =
        normalized.iter().map(|p| memihash(p)).collect();
    for (i, key) in normalized.iter().enumerate() {
        let mut prefix = Vec::new();
        for (part_idx, part) in key.split(|&b| b == b'/').enumerate() {
            if part_idx > 0 {
                prefix.push(b'/');
            }
            prefix.extend_from_slice(part);
            if prefix == *key {
                break;
            }
            let prefix_hash = memihash(&prefix);
            if !full_path_set.contains(&prefix_hash) {
                continue;
            }
            if let Some(others) = by_full.get(&prefix_hash) {
                for &j in others {
                    if normalized[j] == prefix {
                        union(&mut parent, i, j);
                    }
                }
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

    fn spelled_as_on_disk(wt: &Path, rel: &str) -> bool {
        worktree_dentry_spelling(wt, rel).as_deref() == Some(rel)
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
    fn has_dir_ignoring_case_finds_every_leading_directory() {
        let mut index = Index::new();
        index.push_entry_unsorted(dummy_entry("Build/Sub/t.o"));
        index.push_entry_unsorted(dummy_entry("Build/u.o"));
        index.push_entry_unsorted(dummy_entry("README"));
        let tracked = Stage0TrackedPaths::from_index_for_untracked_scan(&index, true);
        for dir in ["build", "BUILD", "Build", "build/sub", "BUILD/SUB"] {
            assert!(tracked.has_dir_ignoring_case(dir.as_bytes()), "{dir}");
        }
        for not_dir in ["readme", "build/t.o", "build/su", "buil", "sub", ""] {
            assert!(
                !tracked.has_dir_ignoring_case(not_dir.as_bytes()),
                "{not_dir}"
            );
        }
        let exact = Stage0TrackedPaths::from_index_for_untracked_scan(&index, false);
        assert!(!exact.has_dir_ignoring_case(b"build"));
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
        let (files, _) = icase_worktree_file_stats(&index, None, true);
        assert_eq!(files, 2);
    }

    #[test]
    fn dentry_spelling_follows_readdir_not_exists_alias() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("file.txt"), b"x").expect("write");
        assert_eq!(
            worktree_dentry_spelling(wt, "file.txt").as_deref(),
            Some("file.txt")
        );
        assert_eq!(
            worktree_dentry_spelling(wt, "File.txt").as_deref(),
            Some("file.txt")
        );
        assert!(spelled_as_on_disk(wt, "file.txt"));
        assert!(!spelled_as_on_disk(wt, "File.txt"));
    }

    fn worktree_has_distinct_icase_dentries(base: &Path) -> bool {
        // Only case may differ, so alias lookup detects filesystem case folding.
        let upper = base.join(".GRIT_ICASE_PROBE");
        let lower = base.join(".grit_icase_probe");
        let _ = std::fs::remove_dir_all(&upper);
        let _ = std::fs::remove_dir_all(&lower);
        if std::fs::create_dir(&upper).is_err() {
            return false;
        }
        let distinct = !lower.exists();
        let _ = std::fs::remove_dir_all(&upper);
        distinct
    }

    #[test]
    fn dentry_spelling_skips_empty_case_duplicate_dir() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        if !worktree_has_distinct_icase_dentries(wt) {
            std::fs::create_dir_all(wt.join("dir")).expect("dir");
            std::fs::write(wt.join("dir/A.txt"), b"x").expect("write");
            assert_eq!(
                worktree_dentry_spelling(wt, "Dir/a.txt").as_deref(),
                Some("dir/A.txt")
            );
            assert!(!spelled_as_on_disk(wt, "Dir/a.txt"));
            return;
        }
        std::fs::create_dir(wt.join("Dir")).expect("Dir");
        std::fs::create_dir_all(wt.join("dir")).expect("dir");
        std::fs::write(wt.join("dir/A.txt"), b"x").expect("write");
        assert_eq!(
            worktree_dentry_spelling(wt, "Dir/a.txt").as_deref(),
            Some("dir/A.txt")
        );
        assert!(!spelled_as_on_disk(wt, "Dir/a.txt"));
    }

    #[test]
    fn dentry_cache_reads_each_directory_once_per_pass() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::create_dir_all(wt.join("d/sub")).expect("mkdir");
        let n = 200;
        for i in 0..n {
            std::fs::write(wt.join(format!("d/f{i:05}.txt")), b"x").expect("write");
        }
        std::fs::write(wt.join("d/sub/Leaf.txt"), b"x").expect("write");
        let mut reads: HashMap<PathBuf, usize> = HashMap::new();
        let mut counting = |dir: &Path| {
            *reads.entry(dir.to_path_buf()).or_default() += 1;
            std::fs::read_dir(dir)
        };
        let mut cache = DentrySpellingCache::default();
        for i in 0..n {
            let rel = format!("d/f{i:05}.txt");
            assert_eq!(
                cache.spelling_with(wt, &rel, &mut counting).as_deref(),
                Some(rel.as_str())
            );
        }
        assert_eq!(
            cache
                .spelling_with(wt, "D/SUB/leaf.TXT", &mut counting)
                .as_deref(),
            Some("d/sub/Leaf.txt")
        );
        assert_eq!(
            cache.spelling_with(wt, "d/missing.txt", &mut counting),
            None
        );
        assert_eq!(cache.spelling_with(wt, "nope/x", &mut counting), None);
        assert_eq!(reads.len(), 3, "{reads:?}");
        assert!(reads.values().all(|&count| count == 1), "{reads:?}");
    }

    #[test]
    fn dentry_cache_prefers_exact_spelling_then_lexicographic() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        if !worktree_has_distinct_icase_dentries(wt) {
            return;
        }
        for name in ["b", "B", "cC", "Cc"] {
            std::fs::create_dir(wt.join(name)).expect("mkdir");
            std::fs::write(wt.join(name).join("x"), b"x").expect("write");
        }
        std::fs::create_dir(wt.join("Empty")).expect("mkdir");
        std::fs::create_dir(wt.join("empty")).expect("mkdir");
        std::fs::write(wt.join("empty/y"), b"y").expect("write");
        let mut cache = DentrySpellingCache::default();
        assert_eq!(cache.spelling(wt, "b/x").as_deref(), Some("b/x"));
        assert_eq!(cache.spelling(wt, "B/x").as_deref(), Some("B/x"));
        assert_eq!(cache.spelling(wt, "cc/x").as_deref(), Some("Cc/x"));
        assert_eq!(cache.spelling(wt, "Empty/y").as_deref(), Some("empty/y"));
        assert_eq!(cache.spelling(wt, "b/X").as_deref(), Some("b/x"));
        assert_eq!(cache.spelling(wt, "b/z"), None);
    }

    #[test]
    fn worktree_path_for_index_entry_uses_direct_path_when_present() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("tracked.txt"), b"x").expect("write");
        let resolved = worktree_path_for_index_entry(wt, "tracked.txt", true);
        assert_eq!(resolved, wt.join("tracked.txt"));
    }

    #[test]
    fn worktree_path_for_index_entry_resolves_missing_direct_spelling() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("file.txt"), b"x").expect("write");
        if wt.join("File.txt").exists() {
            return;
        }
        let resolved = worktree_path_for_index_entry(wt, "File.txt", true);
        assert_eq!(resolved, wt.join("file.txt"));
    }

    #[test]
    fn dentry_spelling_resolves_nested_parent_components() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::create_dir_all(wt.join("dir")).expect("mkdir");
        std::fs::write(wt.join("dir/A.txt"), b"x").expect("write");
        assert_eq!(
            worktree_dentry_spelling(wt, "dir/A.txt").as_deref(),
            Some("dir/A.txt")
        );
        assert!(!spelled_as_on_disk(wt, "Dir/a.txt"));
        assert!(spelled_as_on_disk(wt, "dir/A.txt"));
    }

    #[test]
    fn collision_loser_skips_alias_but_not_worktree_dentry_spelling() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("file.txt"), b"on-disk\n").expect("write");
        let on_disk = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let other = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        let mut index = Index::new();
        let mut file = dummy_entry("file.txt");
        file.oid = on_disk;
        let mut alias = dummy_entry("File.txt");
        alias.oid = other;
        index.push_entry_unsorted(file);
        index.push_entry_unsorted(alias);
        index.sort();
        let map = IcaseCheckoutCollisionMap::from_index(wt, &index, true);
        assert!(!map.is_auto_stage_loser(b"file.txt"));
        assert!(map.is_auto_stage_loser(b"File.txt"));
    }

    #[test]
    fn edited_winner_blob_does_not_make_winner_a_loser() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("file.txt"), b"edited\n").expect("write");
        let mut index = Index::new();
        let mut file = dummy_entry("file.txt");
        file.oid = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let mut alias = dummy_entry("File.txt");
        alias.oid = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        index.push_entry_unsorted(file);
        index.push_entry_unsorted(alias);
        index.sort();
        let map = IcaseCheckoutCollisionMap::from_index(wt, &index, true);
        assert!(!map.is_auto_stage_loser(b"file.txt"));
    }

    #[test]
    #[ignore = "manual perf probe for maintainer reports"]
    fn icase_collision_map_large_index_timing() {
        fn entry(path: &str) -> IndexEntry {
            dummy_entry(path)
        }
        let mut index = Index::new();
        for i in 0..10_000 {
            let base = format!("dir{i}/file.txt");
            index.push_entry_unsorted(entry(&base));
            index.push_entry_unsorted(entry(&base.replace("file", "File")));
        }
        index.sort();
        let wt = std::path::Path::new(".");
        let t0 = std::time::Instant::now();
        let _ = icase_checkout_collision_paths(&index, true);
        let collide_ms = t0.elapsed().as_secs_f64() * 1000.0;
        let t1 = std::time::Instant::now();
        let _ = icase_worktree_file_stats(&index, Some(wt), true);
        let stats_ms = t1.elapsed().as_secs_f64() * 1000.0;
        let t2 = std::time::Instant::now();
        let _ = IcaseCheckoutCollisionMap::from_index(wt, &index, true);
        let map_ms = t2.elapsed().as_secs_f64() * 1000.0;
        eprintln!(
            "icase 20k entries: collide={collide_ms:.2}ms stats={stats_ms:.2}ms map={map_ms:.2}ms"
        );
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

    #[test]
    fn memihash_distinct_collision_groups_keep_separate_auto_stage_winners() {
        assert_eq!(memihash(b"f02398b.txt"), memihash(b"f0688b8.txt"));
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("f02398b.txt"), b"one\n").expect("write");
        std::fs::write(wt.join("f0688b8.txt"), b"two\n").expect("write");
        let mut index = Index::new();
        for path in ["F02398b.txt", "f02398b.txt", "F0688b8.txt", "f0688b8.txt"] {
            index.push_entry_unsorted(dummy_entry(path));
        }
        index.sort();
        let map = IcaseCheckoutCollisionMap::from_index(wt, &index, true);
        assert!(
            !map.is_auto_stage_loser(b"f02398b.txt"),
            "dentry winner must not be treated as a collision loser"
        );
        assert!(
            !map.is_auto_stage_loser(b"f0688b8.txt"),
            "second collision group must not inherit the first group's loser logic"
        );
        assert!(map.is_auto_stage_loser(b"F02398b.txt"));
        assert!(map.is_auto_stage_loser(b"F0688b8.txt"));
    }
}
