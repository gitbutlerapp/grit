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

/// Compare two repository-relative paths for identity.
#[must_use]
pub fn paths_equal(path_a: &[u8], path_b: &[u8], ignorecase: bool) -> bool {
    if path_a == path_b {
        return true;
    }
    ignorecase && path_a.eq_ignore_ascii_case(path_b)
}

fn cmp_ignore_ascii_case(a: &str, b: &str) -> std::cmp::Ordering {
    a.bytes()
        .map(|c| c.to_ascii_lowercase())
        .cmp(b.bytes().map(|c| c.to_ascii_lowercase()))
}

/// Entry names of one work-tree directory, sorted ASCII case-insensitively and then by
/// bytes, so the case variants of a name form one sorted run.
#[derive(Debug, Default)]
struct DirListing(Vec<Box<str>>);

impl DirListing {
    fn read(dir_abs: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(dir_abs) else {
            return Self::default();
        };
        let mut names: Vec<Box<str>> = entries
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into())
            .collect();
        names.sort_unstable_by(|a, b| cmp_ignore_ascii_case(a, b).then_with(|| a.cmp(b)));
        Self(names)
    }

    /// Every entry equal to `want` ignoring ASCII case, in byte order.
    fn case_variants(&self, want: &str) -> &[Box<str>] {
        let start = self
            .0
            .partition_point(|name| cmp_ignore_ascii_case(name, want).is_lt());
        let len = self.0[start..].partition_point(|name| cmp_ignore_ascii_case(name, want).is_eq());
        &self.0[start..start + len]
    }
}

/// The `nth` entry of `variants` in lookup order: the exact spelling `want` first, then
/// the other case variants in byte order.
fn nth_dentry_candidate<'b>(variants: &'b [Box<str>], want: &str, nth: usize) -> Option<&'b str> {
    let exact = variants.iter().position(|name| &**name == want);
    let pos = match (exact, nth) {
        (Some(exact), 0) => exact,
        (Some(exact), n) if n <= exact => n - 1,
        (_, n) => n,
    };
    variants.get(pos).map(|name| &**name)
}

/// Resolves on-disk dentry spellings of repository-relative paths, reading each work-tree
/// directory at most once.
///
/// [`worktree_dentry_spelling`] answers a single lookup; use this cache when resolving many
/// paths in one pass so the cost is one `readdir` per directory instead of one per path
/// component.
#[derive(Debug)]
pub(crate) struct DentrySpellingCache<'w> {
    work_tree: &'w Path,
    /// Listings keyed by the directory's on-disk spelling relative to the work tree (`""`
    /// for the root).
    dirs: hashbrown::HashMap<String, DirListing>,
}

impl<'w> DentrySpellingCache<'w> {
    pub(crate) fn new(work_tree: &'w Path) -> Self {
        Self {
            work_tree,
            dirs: hashbrown::HashMap::new(),
        }
    }

    /// Repository-relative path using each component's on-disk spelling, or `None` when
    /// no case variant of `index_rel` exists in the work tree.
    pub(crate) fn spelling(&mut self, index_rel: &str) -> Option<String> {
        let mut resolved = String::with_capacity(index_rel.len());
        (self.resolve_into(&mut resolved, index_rel) && !resolved.is_empty()).then_some(resolved)
    }

    /// Append the on-disk spelling of the `/`-separated `rest` below directory `prefix` to
    /// `prefix`, skipping empty components.
    ///
    /// Tries every case variant of each component (exact spelling first) so an empty
    /// `Dir/` next to `dir/a.txt` on a case-sensitive volume still resolves `Dir/a.txt`.
    fn resolve_into(&mut self, prefix: &mut String, rest: &str) -> bool {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty() {
            return true;
        }
        let (head, tail) = rest.split_once('/').unwrap_or((rest, ""));
        let base_len = prefix.len();
        for nth in 0.. {
            let variants = self.listing(prefix).case_variants(head);
            let Some(actual) = nth_dentry_candidate(variants, head, nth) else {
                return false;
            };
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(actual);
            if self.resolve_into(prefix, tail) {
                return true;
            }
            prefix.truncate(base_len);
        }
        false
    }

    fn listing(&mut self, dir_rel: &str) -> &DirListing {
        let work_tree = self.work_tree;
        self.dirs
            .entry_ref(dir_rel)
            .or_insert_with(|| DirListing::read(&work_tree.join(dir_rel)))
    }
}

/// Repository-relative path using each parent directory entry's on-disk spelling.
///
/// On case-insensitive volumes this reflects the dentry spelling from `readdir`, not the
/// index's lexical path (Git's `core.ignorecase` behavior).
///
/// # Parameters
///
/// - `work_tree`: root of the work tree.
/// - `index_rel`: `/`-separated path relative to `work_tree`, compared ASCII
///   case-insensitively against directory entries.
///
/// # Returns
///
/// The on-disk spelling of every component (the exact spelling wins when several case
/// variants exist), or `None` when no case variant of the path exists.
#[must_use]
pub fn worktree_dentry_spelling(work_tree: &Path, index_rel: &str) -> Option<String> {
    DentrySpellingCache::new(work_tree).spelling(index_rel)
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
    let paths = stage0_blob_paths(index);
    let collided: BTreeSet<String> = icase_collision_groups(&paths, false)
        .into_iter()
        .flatten()
        .collect();
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
        let grouped = icase_collision_groups(&paths, false);
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
        let mut spellings = DentrySpellingCache::new(work_tree);
        for (key, members) in &groups.members_by_group {
            if members.len() <= 1 {
                continue;
            }
            let winner = resolve_collision_group_dentry_winner(&mut spellings, members);
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

fn stage0_blob_paths(index: &Index) -> Vec<&str> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_TREE && e.mode != MODE_GITLINK)
        .filter_map(|e| std::str::from_utf8(&e.path).ok())
        .collect()
}

fn resolve_collision_group_dentry_winner(
    spellings: &mut DentrySpellingCache<'_>,
    members: &[String],
) -> String {
    if let Some(dentry) = members.iter().find_map(|p| spellings.spelling(p)) {
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
    let groups = icase_collision_groups(&paths, true);
    let mut spellings = work_tree.map(DentrySpellingCache::new);
    for group in groups {
        files += 1;
        let mut group_bytes = 0u64;
        if let (Some(wt), Some(spellings)) = (work_tree, spellings.as_mut()) {
            let probe = if group.len() > 1 {
                resolve_collision_group_dentry_winner(spellings, &group)
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

/// Group `paths` that share one worktree slot when ASCII case is ignored: equal paths, or
/// one path naming a leading directory of the other (Git's `paths_collide`), closed
/// transitively.
///
/// Each group and the list of groups are sorted. Paths that collide with nothing form
/// single-path groups, returned only when `singletons` is set.
fn icase_collision_groups(paths: &[&str], singletons: bool) -> Vec<Vec<String>> {
    fn find(parent: &mut [usize], i: usize) -> usize {
        let mut root = i;
        while parent[root] != root {
            root = parent[root];
        }
        let mut node = i;
        while parent[node] != root {
            let next = parent[node];
            parent[node] = root;
            node = next;
        }
        root
    }

    // Paths bucketed by case-folded hash as singly linked chains: `head` holds the first
    // path of each bucket and `next` the following one, so building costs no per-bucket
    // allocation.
    let mut head: HashMap<u32, usize> = HashMap::with_capacity(paths.len());
    let mut next: Vec<Option<usize>> = Vec::with_capacity(paths.len());
    for (i, path) in paths.iter().enumerate() {
        next.push(head.insert(memihash(path.as_bytes()), i));
    }

    let mut parent: Vec<usize> = (0..paths.len()).collect();
    for (i, path) in paths.iter().enumerate() {
        let bytes = path.as_bytes();
        let leading_dirs = bytes
            .iter()
            .enumerate()
            .filter(|&(_, &b)| b == b'/')
            .map(|(slash, _)| &bytes[..slash]);
        for key in leading_dirs.chain(std::iter::once(bytes)) {
            let mut cursor = head.get(&memihash(key)).copied();
            while let Some(j) = cursor {
                cursor = next[j];
                if j != i && paths[j].as_bytes().eq_ignore_ascii_case(key) {
                    let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                    if ri != rj {
                        parent[rj] = ri;
                    }
                }
            }
        }
    }

    let roots: Vec<usize> = (0..paths.len()).map(|i| find(&mut parent, i)).collect();
    let mut sizes = vec![0usize; paths.len()];
    for &root in &roots {
        sizes[root] += 1;
    }
    let mut by_root: HashMap<usize, Vec<String>> = HashMap::new();
    for (path, &root) in paths.iter().zip(&roots) {
        if singletons || sizes[root] > 1 {
            by_root.entry(root).or_default().push((*path).to_owned());
        }
    }
    let mut groups: Vec<Vec<String>> = by_root.into_values().collect();
    for group in &mut groups {
        group.sort();
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
    fn collision_groups_join_case_aliases_and_dir_file_pairs() {
        let paths = [
            "A/B",
            "a",
            "a/c",
            "File.txt",
            "file.txt",
            "Dir/a.txt",
            "dir/A.txt",
            "dir/b.txt",
            "lone",
            "a.txt",
        ];
        assert_eq!(
            icase_collision_groups(&paths, false),
            vec![
                vec!["A/B".to_owned(), "a".to_owned(), "a/c".to_owned()],
                vec!["Dir/a.txt".to_owned(), "dir/A.txt".to_owned()],
                vec!["File.txt".to_owned(), "file.txt".to_owned()],
            ]
        );
        let all = icase_collision_groups(&paths, true);
        assert_eq!(all.len(), 6);
        assert!(all.contains(&vec!["lone".to_owned()]));
        assert!(all.contains(&vec!["dir/b.txt".to_owned()]));
        assert!(icase_collision_groups(&["a/b", "a/c", "b"], false).is_empty());
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
        assert_eq!(
            worktree_dentry_spelling(wt, "file.txt").as_deref(),
            Some("file.txt")
        );
        assert_ne!(
            worktree_dentry_spelling(wt, "File.txt").as_deref(),
            Some("File.txt")
        );
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
            assert_ne!(
                worktree_dentry_spelling(wt, "Dir/a.txt").as_deref(),
                Some("Dir/a.txt")
            );
            return;
        }
        std::fs::create_dir(wt.join("Dir")).expect("Dir");
        std::fs::create_dir_all(wt.join("dir")).expect("dir");
        std::fs::write(wt.join("dir/A.txt"), b"x").expect("write");
        assert_eq!(
            worktree_dentry_spelling(wt, "Dir/a.txt").as_deref(),
            Some("dir/A.txt")
        );
        assert_ne!(
            worktree_dentry_spelling(wt, "Dir/a.txt").as_deref(),
            Some("Dir/a.txt")
        );
    }

    #[test]
    fn dentry_cache_reads_each_directory_once() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        for d in 0..3 {
            std::fs::create_dir_all(wt.join(format!("d{d}/sub"))).expect("mkdir");
            for f in 0..20 {
                std::fs::write(wt.join(format!("d{d}/sub/f{f}.txt")), b"x").expect("write");
            }
        }
        let mut cache = DentrySpellingCache::new(wt);
        for d in 0..3 {
            for f in 0..20 {
                let rel = format!("d{d}/sub/f{f}.txt");
                assert_eq!(cache.spelling(&rel).as_deref(), Some(rel.as_str()));
            }
        }
        assert_eq!(cache.dirs.len(), 1 + 3 + 3, "root, d*, d*/sub");
        assert_eq!(cache.spelling("d0/sub/missing.txt"), None);
        assert_eq!(cache.spelling("nope/f0.txt"), None);
        assert_eq!(cache.spelling(""), None);
        assert_eq!(cache.dirs.len(), 1 + 3 + 3, "missing paths reuse listings");
    }

    #[test]
    fn dentry_cache_matches_one_off_lookup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::create_dir_all(wt.join("Src/Deep")).expect("mkdir");
        std::fs::write(wt.join("Src/Deep/Main.rs"), b"x").expect("write");
        std::fs::write(wt.join("README"), b"x").expect("write");
        let mut cache = DentrySpellingCache::new(wt);
        for rel in [
            "src/deep/main.rs",
            "SRC/DEEP/MAIN.RS",
            "Src/Deep/Main.rs",
            "readme",
            "src/deep",
            "src/other.rs",
            "/src//deep/main.rs",
        ] {
            assert_eq!(
                cache.spelling(rel),
                worktree_dentry_spelling(wt, rel),
                "{rel}"
            );
        }
        assert_eq!(
            cache.spelling("src/deep/main.rs").as_deref(),
            Some("Src/Deep/Main.rs")
        );
    }

    #[test]
    fn dentry_candidates_put_exact_spelling_first() {
        let bucket: Vec<Box<str>> = ["A.txt", "a.TXT", "a.txt"]
            .into_iter()
            .map(Box::from)
            .collect();
        let order: Vec<&str> = (0..)
            .map_while(|n| nth_dentry_candidate(&bucket, "a.TXT", n))
            .collect();
        assert_eq!(order, ["a.TXT", "A.txt", "a.txt"]);
        let order: Vec<&str> = (0..)
            .map_while(|n| nth_dentry_candidate(&bucket, "A.TXT", n))
            .collect();
        assert_eq!(order, ["A.txt", "a.TXT", "a.txt"]);
        let order: Vec<&str> = (0..)
            .map_while(|n| nth_dentry_candidate(&bucket, "a.txt", n))
            .collect();
        assert_eq!(order, ["a.txt", "A.txt", "a.TXT"]);
    }

    #[test]
    fn dir_listing_case_variants_are_one_sorted_run() {
        let mut names: Vec<Box<str>> = ["b", "a.txt", "A.TXT", "a.txtx", "a", "A.txt", "_"]
            .into_iter()
            .map(Box::from)
            .collect();
        names.sort_unstable_by(|a, b| cmp_ignore_ascii_case(a, b).then_with(|| a.cmp(b)));
        let listing = DirListing(names);
        let variants: Vec<&str> = listing
            .case_variants("a.Txt")
            .iter()
            .map(|n| &**n)
            .collect();
        assert_eq!(variants, ["A.TXT", "A.txt", "a.txt"]);
        assert!(listing.case_variants("c").is_empty());
        assert_eq!(listing.case_variants("A").len(), 1);
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
        assert_ne!(
            worktree_dentry_spelling(wt, "Dir/a.txt").as_deref(),
            Some("Dir/a.txt")
        );
        assert_eq!(
            worktree_dentry_spelling(wt, "dir/A.txt").as_deref(),
            Some("dir/A.txt")
        );
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
