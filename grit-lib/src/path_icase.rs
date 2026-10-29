//! Case-insensitive path identity when `core.ignorecase` is enabled.

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use smallvec::SmallVec;

use crate::index::{Index, MODE_GITLINK, MODE_TREE};
use crate::index_name_hash_lazy::memihash;
use crate::parallel::{par_map, ParallelOptions};

/// Names stored back to back in one buffer, with `(case-folded hash, start, end)` spans
/// sorted by hash: two allocations however many names, and lookups by binary search.
#[derive(Debug, Default)]
struct FoldedNames {
    bytes: Vec<u8>,
    spans: Vec<(u32, usize, usize)>,
}

impl FoldedNames {
    fn push(&mut self, name: &[u8]) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(name);
        self.spans.push((memihash(name), start, self.bytes.len()));
    }

    /// Make the pushed names searchable; call once after the last [`Self::push`].
    fn seal(&mut self) {
        self.spans.sort_unstable_by_key(|&(hash, _, _)| hash);
    }

    /// Names whose case-folded hash equals that of `name` (a superset of its case variants).
    fn same_fold_hash<'s>(&'s self, name: &[u8]) -> impl Iterator<Item = &'s [u8]> + 's {
        let hash = memihash(name);
        let first = self.spans.partition_point(|&(h, _, _)| h < hash);
        self.spans[first..]
            .iter()
            .take_while(move |&&(h, _, _)| h == hash)
            .map(|&(_, start, end)| &self.bytes[start..end])
    }
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

/// Stage-0 tracked paths for fast lookups during untracked scans.
///
/// Borrows from the [`Index`] it was built from, so building the table does not copy every
/// indexed path.
#[derive(Debug)]
pub struct Stage0TrackedPaths<'a> {
    exact: ExactPaths<'a>,
    folded: Option<HashMap<u32, Vec<&'a [u8]>>>,
    /// Leading directories of stage-0 entries, bucketed by case-folded hash (`ignorecase` only).
    folded_dirs: Option<HashMap<u32, Vec<&'a [u8]>>>,
}

/// Exact-case membership for [`Stage0TrackedPaths`].
#[derive(Debug)]
enum ExactPaths<'a> {
    /// Binary search in the (sorted) index itself: nothing to build before the walk starts.
    Sorted(&'a Index),
    /// Hash set over the paths of an index whose entries are not known to be sorted.
    Hashed(hashbrown::HashSet<&'a [u8]>),
    /// Case-insensitive tables answer every lookup.
    Unused,
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
                exact: ExactPaths::Unused,
                folded: Some(folded),
                folded_dirs: Some(folded_dirs),
            }
        } else {
            let exact = if index.entries_sorted {
                ExactPaths::Sorted(index)
            } else {
                ExactPaths::Hashed(index.entries.iter().map(|e| e.path.as_slice()).collect())
            };
            Self {
                exact,
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
            return folded
                .get(&memihash(path))
                .is_some_and(|bucket| bucket_has_path(bucket, path));
        }
        match &self.exact {
            ExactPaths::Sorted(index) => index.sorted_contains_path(path).unwrap_or(false),
            ExactPaths::Hashed(paths) => paths.contains(path),
            ExactPaths::Unused => false,
        }
    }
}

/// Stage-0 index spellings snapshotted for case-insensitive replacement (built once per
/// staging batch).
#[derive(Debug, Default)]
pub struct Stage0IcasePathMap {
    paths: FoldedNames,
}

impl Stage0IcasePathMap {
    /// Snapshot stage-0 paths for case-insensitive replacement during `git add`.
    #[must_use]
    pub fn from_index(index: &Index, ignorecase: bool) -> Self {
        let mut paths = FoldedNames::default();
        if ignorecase {
            let stage0 = index.entries.iter().filter(|e| e.stage() == 0);
            paths
                .bytes
                .reserve(stage0.clone().map(|e| e.path.len()).sum());
            paths.spans.reserve(stage0.clone().count());
            for e in stage0 {
                paths.push(&e.path);
            }
            paths.seal();
        }
        Self { paths }
    }

    /// Remove the index entry that aliases `path` on a case-insensitive filesystem.
    pub fn remove_alias_of(&self, index: &mut Index, path: &[u8]) -> bool {
        let mut removed = false;
        for index_path in self.paths.same_fold_hash(path) {
            if index_path.eq_ignore_ascii_case(path) && index.remove(index_path) {
                removed = true;
            }
        }
        removed
    }

    /// Stage-0 path spelling already recorded for a case variant of `path`, if any.
    #[must_use]
    pub fn existing_stage0_spelling(&self, index: &Index, path: &str) -> Option<String> {
        for index_path in self.paths.same_fold_hash(path.as_bytes()) {
            if index_path.eq_ignore_ascii_case(path.as_bytes())
                && index.get(index_path, 0).is_some()
            {
                return std::str::from_utf8(index_path).ok().map(str::to_owned);
            }
        }
        None
    }
}

/// Stage-0 directory prefix spellings for ignorecase staging (first index order wins).
#[derive(Debug, Default)]
pub(crate) struct Stage0IcaseDirSpellingMap {
    by_fold: HashMap<u32, Vec<Vec<u8>>>,
}

impl Stage0IcaseDirSpellingMap {
    #[must_use]
    pub fn from_index(index: &Index, ignorecase: bool) -> Self {
        let mut by_fold = HashMap::new();
        if !ignorecase {
            return Self { by_fold };
        }
        for e in index.entries.iter().filter(|e| e.stage() == 0) {
            let path = &e.path;
            let mut end = 0usize;
            while let Some(slash) = path[end..].iter().position(|&b| b == b'/') {
                end += slash;
                let prefix = &path[..end];
                let bucket = by_fold.entry(memihash(prefix)).or_default();
                if !bucket
                    .iter()
                    .any(|existing| existing.eq_ignore_ascii_case(prefix))
                {
                    bucket.push(prefix.to_vec());
                }
                end += 1;
            }
        }
        Self { by_fold }
    }

    fn canonical_dir_prefix<'a>(&'a self, prefix: &str) -> Option<&'a [u8]> {
        self.by_fold
            .get(&memihash(prefix.as_bytes()))?
            .iter()
            .find(|candidate| candidate.eq_ignore_ascii_case(prefix.as_bytes()))
            .map(|p| p.as_slice())
    }

    /// Fold directory prefixes of `rel_path` to spellings already in the index.
    #[must_use]
    pub fn fold_directory_prefixes(&self, rel_path: &str) -> String {
        if self.by_fold.is_empty() || rel_path.is_empty() {
            return rel_path.to_owned();
        }
        let parts: Vec<&str> = rel_path.split('/').collect();
        if parts.is_empty() {
            return String::new();
        }
        let mut prefix = String::new();
        for (i, comp) in parts.iter().enumerate() {
            if i > 0 {
                prefix.push('/');
            }
            prefix.push_str(comp);
            if i + 1 < parts.len() {
                if let Some(canonical) = self.canonical_dir_prefix(&prefix) {
                    prefix = String::from_utf8_lossy(canonical).into_owned();
                }
            }
        }
        prefix
    }
}

static FS_PROBE_COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_fs_probe_basename() -> String {
    let seq = FS_PROBE_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!(".grit-icase-fs-probe-{}-{seq}", std::process::id())
}

/// True when the work tree volume folds ASCII path case (APFS, NTFS, …).
///
/// Creates a uniquely named empty file with [`OpenOptions::create_new`]; neither the lowercase
/// nor uppercase spelling may already exist. Only a probe file this call created is removed.
#[must_use]
pub(crate) fn worktree_filesystem_case_insensitive(work_tree: &Path) -> bool {
    for _ in 0..32 {
        let name = unique_fs_probe_basename();
        let upper_name: String = name.chars().map(|c| c.to_ascii_uppercase()).collect();
        let lower = work_tree.join(&name);
        let upper = work_tree.join(&upper_name);
        if lower.exists() || upper.exists() {
            continue;
        }
        if let Err(e) = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&lower)
        {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                continue;
            }
            return false;
        }
        let folds = upper.exists();
        let _ = std::fs::remove_file(&lower);
        return folds;
    }
    false
}

/// Path to use when staging an untracked or new file under `core.ignorecase`.
#[must_use]
pub(crate) fn fold_untracked_staging_path(
    dir_map: &Stage0IcaseDirSpellingMap,
    icase_map: &Stage0IcasePathMap,
    index: &Index,
    rel_path: &str,
) -> String {
    if let Some(existing) = icase_map.existing_stage0_spelling(index, rel_path) {
        return existing;
    }
    dir_map.fold_directory_prefixes(rel_path)
}

/// Compare two repository-relative paths for identity.
#[must_use]
pub fn paths_equal(path_a: &[u8], path_b: &[u8], ignorecase: bool) -> bool {
    if path_a == path_b {
        return true;
    }
    ignorecase && path_a.eq_ignore_ascii_case(path_b)
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
    DentrySpellingCache::new(work_tree)
        .spelling(index_rel)
        .map(Cow::into_owned)
}

type ReadDirFn<'r> = dyn FnMut(&Path) -> std::io::Result<std::fs::ReadDir> + 'r;

/// Fewer parent directories than this are read serially by
/// [`DentrySpellingCache::prefetch_parent_dirs`].
const PREFETCH_PARALLEL_MIN_DIRS: usize = 32;

/// Entry names of one work-tree directory, searchable by case-folded hash.
#[derive(Debug, Default)]
struct DirEntryNames(FoldedNames);

impl DirEntryNames {
    /// Names from a directory read; empty when it failed, like a missing directory.
    fn from_read_dir(read: std::io::Result<std::fs::ReadDir>) -> Self {
        let mut names = FoldedNames::default();
        if let Ok(entries) = read {
            for entry in entries.flatten() {
                names.push(entry.file_name().to_string_lossy().as_bytes());
            }
        }
        names.seal();
        Self(names)
    }

    fn bucket<'s>(&'s self, want: &str) -> impl Iterator<Item = &'s str> + 's {
        self.0
            .same_fold_hash(want.as_bytes())
            .filter_map(|name| std::str::from_utf8(name).ok())
    }

    fn contains(&self, want: &str) -> bool {
        self.bucket(want).any(|name| name == want)
    }

    /// Names equal to `want` ignoring ASCII case: the exact spelling first, then byte order.
    fn candidates(&self, want: &str) -> SmallVec<[String; 1]> {
        let mut names: SmallVec<[String; 1]> = self
            .bucket(want)
            .filter(|name| name.eq_ignore_ascii_case(want))
            .map(str::to_owned)
            .collect();
        names.sort_by(|a, b| (b == want).cmp(&(a == want)).then_with(|| a.cmp(b)));
        names
    }
}

/// Directory listings memoized for one pass over the work tree.
///
/// Resolving the on-disk spelling of a path needs each parent's entries; reading them once per
/// directory keeps a pass over `n` paths linear instead of `n × files-per-directory`, and a
/// path that exists exactly as indexed is answered from its parent's listing without
/// allocating. Gives the same answer as [`worktree_dentry_spelling`] for every path. The cache
/// does not observe later work tree changes, so build a fresh one for every pass.
#[derive(Debug)]
pub(crate) struct DentrySpellingCache<'w> {
    work_tree: &'w Path,
    /// Listings keyed by work-tree-relative directory as spelled on disk (`""` is the root).
    listings: HashMap<String, Arc<DirEntryNames>>,
    /// Last parent directory whose every component exists with the exact index spelling.
    exact_parent: Option<(String, Arc<DirEntryNames>)>,
}

impl<'w> DentrySpellingCache<'w> {
    /// Cache for paths under `work_tree`; nothing is read until a path is resolved.
    #[must_use]
    pub(crate) fn new(work_tree: &'w Path) -> Self {
        Self {
            work_tree,
            listings: HashMap::new(),
            exact_parent: None,
        }
    }

    /// Read every leading directory of `index_paths` (and the work-tree root) ahead of
    /// [`Self::spelling`], using up to `threads` workers when there are enough directories.
    ///
    /// Purely an optimization: directories are read on demand otherwise.
    pub(crate) fn prefetch_parent_dirs<'p>(
        &mut self,
        index_paths: impl IntoIterator<Item = &'p str>,
        threads: NonZeroUsize,
    ) {
        let mut dirs: hashbrown::HashSet<&[u8]> = hashbrown::HashSet::new();
        dirs.insert(b"");
        for path in index_paths {
            insert_leading_dirs(&mut dirs, path.as_bytes());
        }
        let missing: Vec<&str> = dirs
            .into_iter()
            .filter_map(|dir| std::str::from_utf8(dir).ok())
            .filter(|dir| !self.listings.contains_key(*dir))
            .collect();
        let options = ParallelOptions {
            threads,
            min_items: PREFETCH_PARALLEL_MIN_DIRS,
            min_total_bytes: None,
            signed_entry_threshold: None,
        };
        let work_tree = self.work_tree;
        let read = par_map(&missing, options, 0, |dir| {
            DirEntryNames::from_read_dir(std::fs::read_dir(work_tree.join(dir)))
        });
        for (dir, names) in missing.into_iter().zip(read) {
            self.listings.insert(dir.to_owned(), Arc::new(names));
        }
    }

    fn listing(&mut self, rel_dir: &str, read_dir: &mut ReadDirFn<'_>) -> Arc<DirEntryNames> {
        if let Some(names) = self.listings.get(rel_dir) {
            return Arc::clone(names);
        }
        let names = Arc::new(DirEntryNames::from_read_dir(read_dir(
            &self.work_tree.join(rel_dir),
        )));
        self.listings.insert(rel_dir.to_owned(), Arc::clone(&names));
        names
    }

    /// On-disk spelling of the repository-relative `index_rel`, matching each component
    /// ignoring ASCII case and preferring the exact spelling; `None` when nothing matches.
    ///
    /// Borrows `index_rel` when the path exists exactly as spelled, the common case.
    pub(crate) fn spelling<'p>(&mut self, index_rel: &'p str) -> Option<Cow<'p, str>> {
        self.spelling_with(index_rel, &mut |dir: &Path| std::fs::read_dir(dir))
    }

    fn spelling_with<'p>(
        &mut self,
        index_rel: &'p str,
        read_dir: &mut ReadDirFn<'_>,
    ) -> Option<Cow<'p, str>> {
        if self.exists_exactly(index_rel, read_dir) {
            return Some(Cow::Borrowed(index_rel));
        }
        let components: SmallVec<[&str; 8]> =
            index_rel.split('/').filter(|c| !c.is_empty()).collect();
        if components.is_empty() {
            return None;
        }
        self.resolve_under("", &components, read_dir)
            .map(Cow::Owned)
    }

    /// Whether every component of `index_rel` is listed with exactly this spelling.
    fn exists_exactly(&mut self, index_rel: &str, read_dir: &mut ReadDirFn<'_>) -> bool {
        let (parent, leaf) = index_rel.rsplit_once('/').unwrap_or(("", index_rel));
        if leaf.is_empty() {
            return false;
        }
        if let Some((exact_parent, names)) = &self.exact_parent {
            if exact_parent == parent {
                return names.contains(leaf);
            }
        }
        if !parent.is_empty() {
            let mut dir_end = 0;
            for component in parent.split('/') {
                let dir = if dir_end == 0 {
                    ""
                } else {
                    &parent[..dir_end - 1]
                };
                if component.is_empty() || !self.listing(dir, read_dir).contains(component) {
                    return false;
                }
                dir_end += component.len() + 1;
            }
        }
        let names = self.listing(parent, read_dir);
        let found = names.contains(leaf);
        self.exact_parent = Some((parent.to_owned(), names));
        found
    }

    fn resolve_under(
        &mut self,
        parent: &str,
        components: &[&str],
        read_dir: &mut ReadDirFn<'_>,
    ) -> Option<String> {
        let [head, tail @ ..] = components else {
            return None;
        };
        for actual in self.listing(parent, read_dir).candidates(head) {
            let child = if parent.is_empty() {
                actual
            } else {
                format!("{parent}/{actual}")
            };
            if tail.is_empty() {
                return Some(child);
            }
            if let Some(full) = self.resolve_under(&child, tail, read_dir) {
                return Some(full);
            }
        }
        None
    }
}

/// Resolve a worktree path for an index entry, honoring case-insensitive filesystems.
#[must_use]
pub fn worktree_path_for_index_entry(
    work_tree: &Path,
    index_rel: &str,
    ignorecase: bool,
) -> PathBuf {
    let direct = work_tree.join(index_rel);
    if !ignorecase {
        return direct;
    }
    crate::worktree_scan::probe_worktree_path_resolution(&direct);
    if direct.exists() {
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
    let collided: BTreeSet<&str> = icase_collision_sets(&paths)
        .into_iter()
        .flatten()
        .map(|i| paths[i])
        .collect();
    collided.into_iter().map(str::to_owned).collect()
}

/// Stage-0 checkout collision groups keyed by a stable group id (built once per pass).
///
/// Only paths that share a worktree slot with another path are recorded, so an index
/// without collisions costs one hash pass and no per-path allocation.
#[derive(Debug, Default)]
pub(crate) struct IcaseIndexCollisionGroups {
    path_to_group: HashMap<Vec<u8>, usize>,
    /// Members of each group (at least two), sorted by path bytes.
    members_by_group: Vec<Vec<String>>,
}

impl IcaseIndexCollisionGroups {
    pub(crate) fn from_index(index: &Index, ignorecase: bool) -> Self {
        if !ignorecase {
            return Self::default();
        }
        let paths = stage0_blob_paths(index);
        let mut path_to_group = HashMap::new();
        let members_by_group = icase_collision_sets(&paths)
            .into_iter()
            .enumerate()
            .map(|(key, set)| {
                set.into_iter()
                    .map(|i| {
                        path_to_group.insert(paths[i].as_bytes().to_vec(), key);
                        paths[i].to_owned()
                    })
                    .collect()
            })
            .collect();
        Self {
            path_to_group,
            members_by_group,
        }
    }

    pub(crate) fn has_collision_sibling(&self, index_rel: &str) -> bool {
        self.path_to_group.contains_key(index_rel.as_bytes())
    }
}

/// Ignorecase checkout collision map for auto-stage: group membership and dentry winner.
#[derive(Debug, Default)]
pub(crate) struct IcaseCheckoutCollisionMap {
    groups: IcaseIndexCollisionGroups,
    /// Dentry winner of each group, indexed like `groups.members_by_group`.
    winner_by_group: Vec<String>,
    /// Stage-0 blob oids for each group member, parallel to `groups.members_by_group`.
    oids_by_group: Vec<Vec<crate::objects::ObjectId>>,
}

impl IcaseCheckoutCollisionMap {
    pub(crate) fn from_index(work_tree: &Path, index: &Index, ignorecase: bool) -> Self {
        let groups = IcaseIndexCollisionGroups::from_index(index, ignorecase);
        let mut dentries = DentrySpellingCache::new(work_tree);
        let mut winner_by_group = Vec::with_capacity(groups.members_by_group.len());
        let mut oids_by_group = Vec::with_capacity(groups.members_by_group.len());
        for members in &groups.members_by_group {
            winner_by_group.push(resolve_collision_group_dentry_winner(
                members,
                &mut dentries,
            ));
            let oids: Vec<_> = members
                .iter()
                .filter_map(|path| index.get(path.as_bytes(), 0).map(|e| e.oid))
                .collect();
            debug_assert_eq!(oids.len(), members.len());
            oids_by_group.push(oids);
        }
        Self {
            groups,
            winner_by_group,
            oids_by_group,
        }
    }

    /// Worktree bytes match a collision sibling's index blob, not this row's.
    ///
    /// On a shared dentry the winner spelling can still hash to a sibling's oid; leave the
    /// index row unchanged so auto-stage does not rewrite the loser's recorded blob.
    pub(crate) fn worktree_oid_is_collision_sibling_blob(
        &self,
        raw_path: &[u8],
        worktree_oid: crate::objects::ObjectId,
        index_oid: crate::objects::ObjectId,
    ) -> bool {
        if worktree_oid == index_oid {
            return false;
        }
        let Some(&key) = self.groups.path_to_group.get(raw_path) else {
            return false;
        };
        let members = &self.groups.members_by_group[key];
        let oids = &self.oids_by_group[key];
        members
            .iter()
            .zip(oids.iter())
            .any(|(member, oid)| member.as_bytes() != raw_path && *oid == worktree_oid)
    }

    /// Collision paths must hash worktree content before reporting modified (size alone lies).
    pub(crate) fn diff_requires_worktree_hash(&self, raw_path: &[u8]) -> bool {
        self.groups.path_to_group.contains_key(raw_path) && !self.is_auto_stage_loser(raw_path)
    }

    /// Skip auto-stage refresh for collision aliases; the dentry winner is never a loser.
    pub(crate) fn is_auto_stage_loser(&self, raw_path: &[u8]) -> bool {
        let Some(&key) = self.groups.path_to_group.get(raw_path) else {
            return false;
        };
        self.winner_by_group
            .get(key)
            .is_some_and(|winner| raw_path != winner.as_bytes())
    }

    /// Losers stay in the index when the worktree file is missing; winners may be removed.
    pub(crate) fn preserve_missing_collision_loser(&self, raw_path: &[u8]) -> bool {
        self.is_auto_stage_loser(raw_path)
    }

    pub(crate) fn collision_groups(&self) -> &IcaseIndexCollisionGroups {
        &self.groups
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

fn resolve_collision_group_dentry_winner<S: AsRef<str>>(
    members: &[S],
    dentries: &mut DentrySpellingCache<'_>,
) -> String {
    if let Some(dentry) = members
        .iter()
        .find_map(|p| dentries.spelling(p.as_ref()).map(Cow::into_owned))
    {
        if let Some(exact) = members.iter().find(|m| m.as_ref() == dentry) {
            return exact.as_ref().to_owned();
        }
    }
    icase_group_fallback_winner(members)
}

fn icase_group_fallback_winner<S: AsRef<str>>(members: &[S]) -> String {
    let fold = members[0].as_ref().to_ascii_lowercase();
    if let Some(exact) = members.iter().find(|m| m.as_ref() == fold) {
        return exact.as_ref().to_owned();
    }
    let mut candidates: Vec<&str> = members
        .iter()
        .map(AsRef::as_ref)
        .filter(|m| m.eq_ignore_ascii_case(&fold))
        .collect();
    candidates.sort_by(|a, b| compare_icase_spelling_preference(a, b));
    candidates
        .first()
        .copied()
        .unwrap_or_else(|| members[0].as_ref())
        .to_owned()
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
    let sets = icase_collision_sets(&paths);
    let mut in_set = vec![false; paths.len()];
    for &i in sets.iter().flatten() {
        in_set[i] = true;
    }
    let mut dentries = work_tree.map(DentrySpellingCache::new);
    let mut group_bytes = |group: &[&str]| -> u64 {
        let mut group_bytes = 0u64;
        if let (Some(wt), Some(dentries)) = (work_tree, dentries.as_mut()) {
            let probe = if group.len() > 1 {
                resolve_collision_group_dentry_winner(group, dentries)
            } else {
                group[0].to_owned()
            };
            let abs = worktree_path_for_index_entry(wt, &probe, true);
            if abs.is_file() {
                if let Ok(meta) = std::fs::symlink_metadata(&abs) {
                    group_bytes = meta.len();
                }
            }
        }
        if group_bytes == 0 {
            for path in group {
                if let Some(e) = index.get(path.as_bytes(), 0) {
                    group_bytes = group_bytes.max(u64::from(e.size));
                }
            }
        }
        group_bytes
    };
    for (i, path) in paths.iter().enumerate() {
        if !in_set[i] {
            files += 1;
            bytes += group_bytes(&[path]);
        }
    }
    for set in &sets {
        let group: Vec<&str> = set.iter().map(|&i| paths[i]).collect();
        files += 1;
        bytes += group_bytes(&group);
    }
    (files, bytes)
}

/// Union-find over `0..len`, allocated on the first union so collision-free inputs stay free.
struct LazyDisjointSets {
    len: usize,
    parent: Vec<usize>,
}

impl LazyDisjointSets {
    fn new(len: usize) -> Self {
        Self {
            len,
            parent: Vec::new(),
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        if self.parent.is_empty() {
            self.parent = (0..self.len).collect();
        }
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[rb] = ra;
        }
    }

    /// Sets with more than one member, each in ascending order.
    fn into_multi_member_sets(mut self) -> Vec<Vec<usize>> {
        if self.parent.is_empty() {
            return Vec::new();
        }
        let mut by_root: HashMap<usize, Vec<usize>> = HashMap::new();
        for i in 0..self.len {
            let root = self.find(i);
            by_root.entry(root).or_default().push(i);
        }
        by_root.into_values().filter(|s| s.len() > 1).collect()
    }
}

/// Positions in `paths` that share a worktree slot under `core.ignorecase`: paths equal
/// ignoring ASCII case, and a file whose path is a leading directory of another path.
///
/// Only groups with at least two members are returned, each sorted by path bytes.
fn icase_collision_sets(paths: &[&str]) -> Vec<Vec<usize>> {
    let mut sets = LazyDisjointSets::new(paths.len());
    let mut by_fold: HashMap<u32, SmallVec<[usize; 1]>> = HashMap::with_capacity(paths.len());
    for (i, path) in paths.iter().enumerate() {
        by_fold
            .entry(memihash(path.as_bytes()))
            .or_default()
            .push(i);
    }
    for bucket in by_fold.values().filter(|b| b.len() > 1) {
        for (a, &i) in bucket.iter().enumerate() {
            for &j in &bucket[a + 1..] {
                if paths[i].eq_ignore_ascii_case(paths[j]) {
                    sets.union(i, j);
                }
            }
        }
    }

    let mut dirs = hashbrown::HashSet::new();
    for path in paths {
        insert_leading_dirs(&mut dirs, path.as_bytes());
    }
    let file_at_dir: HashMap<&[u8], usize> = dirs
        .into_iter()
        .filter_map(|dir| {
            let bucket = by_fold.get(&memihash(dir))?;
            let file = bucket
                .iter()
                .find(|&&j| paths[j].as_bytes().eq_ignore_ascii_case(dir))?;
            Some((dir, *file))
        })
        .collect();
    if !file_at_dir.is_empty() {
        for (i, path) in paths.iter().enumerate() {
            let path = path.as_bytes();
            for (slash, _) in path.iter().enumerate().filter(|(_, b)| **b == b'/') {
                if let Some(&file) = file_at_dir.get(&path[..slash]) {
                    sets.union(i, file);
                }
            }
        }
    }
    let mut sets = sets.into_multi_member_sets();
    for set in &mut sets {
        set.sort_by_key(|&i| paths[i]);
    }
    sets.sort_by_key(|set| paths[set[0]]);
    sets
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;
    use crate::objects::ObjectId;

    #[test]
    fn fold_untracked_staging_path_uses_index_dir_and_leaf_spelling() {
        let mut index = Index::default();
        for path in ["Dir/f.txt", "readme.txt"] {
            index.add_or_replace(dummy_entry(path));
        }
        let icase_map = Stage0IcasePathMap::from_index(&index, true);
        let dir_map = Stage0IcaseDirSpellingMap::from_index(&index, true);
        assert_eq!(
            fold_untracked_staging_path(&dir_map, &icase_map, &index, "dir/g.txt"),
            "Dir/g.txt"
        );
        assert_eq!(
            fold_untracked_staging_path(&dir_map, &icase_map, &index, "README.txt"),
            "readme.txt"
        );
    }

    #[test]
    fn dir_spelling_map_does_not_fold_across_memihash_collisions() {
        assert_eq!(memihash(b"f02398b.txt"), memihash(b"f0688b8.txt"));
        let mut index = Index::default();
        index.add_or_replace(dummy_entry("f02398b.txt/x"));
        let dir_map = Stage0IcaseDirSpellingMap::from_index(&index, true);
        assert_eq!(
            dir_map.fold_directory_prefixes("f0688b8.txt/y"),
            "f0688b8.txt/y"
        );
    }

    #[test]
    fn filesystem_case_probe_does_not_remove_preexisting_paths() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        let user_dir = wt.join(".grit_icase_probe");
        std::fs::create_dir_all(user_dir.join("sentinel")).expect("user dir");
        let _ = worktree_filesystem_case_insensitive(wt);
        assert!(
            user_dir.join("sentinel").is_dir(),
            "probe must not delete an existing .grit_icase_probe tree"
        );
    }

    #[test]
    fn filesystem_case_probe_ignores_preexisting_uppercase_decoy() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        if !worktree_has_distinct_icase_dentries(wt) {
            return;
        }
        std::fs::write(wt.join(".GRIT-ICASE-FS-PROBE-00000"), b"decoy").expect("decoy");
        assert!(
            !worktree_filesystem_case_insensitive(wt),
            "a pre-existing uppercase path must not classify the volume as case-folding"
        );
        assert!(
            wt.join(".GRIT-ICASE-FS-PROBE-00000").is_file(),
            "probe must not remove unrelated paths"
        );
    }

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

    fn collision_sets_of<'a>(paths: &[&'a str]) -> Vec<Vec<&'a str>> {
        icase_collision_sets(paths)
            .into_iter()
            .map(|set| set.into_iter().map(|i| paths[i]).collect())
            .collect()
    }

    #[test]
    fn collision_sets_group_case_variants_and_file_dir_overlaps() {
        assert!(collision_sets_of(&["a/b", "a/c", "b"]).is_empty());
        assert_eq!(
            collision_sets_of(&["File.txt", "file.txt", "other"]),
            vec![vec!["File.txt", "file.txt"]]
        );
        assert_eq!(
            collision_sets_of(&["A/B", "a", "x/y"]),
            vec![vec!["A/B", "a"]]
        );
        assert_eq!(
            collision_sets_of(&["Dir/a.txt", "dir/A.txt", "dir/b"]),
            vec![vec!["Dir/a.txt", "dir/A.txt"]]
        );
        assert_eq!(
            collision_sets_of(&["a", "A/b", "a/c/d", "z"]),
            vec![vec!["A/b", "a", "a/c/d"]],
            "a file collides with every path under a case variant of its name"
        );
        assert!(
            collision_sets_of(&["ab/c", "a"]).is_empty(),
            "a path prefix that is not a whole directory does not collide"
        );
    }

    #[test]
    fn collision_sets_ignore_hash_collisions_between_distinct_paths() {
        assert_eq!(memihash(b"f02398b.txt"), memihash(b"f0688b8.txt"));
        assert!(collision_sets_of(&["f02398b.txt", "f0688b8.txt"]).is_empty());
        assert_eq!(
            collision_sets_of(&["f02398b.txt/x", "f0688b8.txt"]),
            Vec::<Vec<&str>>::new()
        );
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
    fn exact_tracked_paths_answer_from_sorted_and_unsorted_indexes() {
        let mut conflicted = dummy_entry("dir/both.txt");
        conflicted.set_stage(2);
        let mut sorted = Index::new();
        for entry in [dummy_entry("dir/a.txt"), conflicted, dummy_entry("README")] {
            sorted.push_entry_unsorted(entry);
        }
        let mut unsorted = Index::new();
        unsorted.entries = sorted.entries.clone();
        unsorted.mark_entries_unsorted();
        sorted.sort();
        for index in [&sorted, &unsorted] {
            let tracked = Stage0TrackedPaths::from_index_for_untracked_scan(index, false);
            for path in ["dir/a.txt", "dir/both.txt", "README"] {
                assert!(tracked.contains_bytes(path.as_bytes()), "{path}");
            }
            for path in ["dir", "dir/A.txt", "readme", "dir/a", ""] {
                assert!(!tracked.contains_bytes(path.as_bytes()), "{path}");
            }
        }
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

    fn spelled_as_on_disk(wt: &Path, rel: &str) -> bool {
        worktree_dentry_spelling(wt, rel).as_deref() == Some(rel)
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
    fn dentry_spellings_read_each_directory_once_and_match_one_shot_lookup() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        let mut paths = Vec::new();
        for dir in ["a/b", "a/c", "Top"] {
            std::fs::create_dir_all(wt.join(dir)).expect("mkdir");
            for file in ["one", "Two"] {
                std::fs::write(wt.join(dir).join(file), b"x").expect("write");
                paths.push(format!("{dir}/{file}"));
            }
        }
        std::fs::write(wt.join("root.txt"), b"x").expect("write");
        paths.push("root.txt".to_owned());
        let queries: Vec<String> = paths
            .iter()
            .flat_map(|p| [p.clone(), p.to_ascii_lowercase(), p.to_ascii_uppercase()])
            .chain(["a/b/missing".to_owned(), "nope/x".to_owned(), String::new()])
            .collect();

        let mut spellings = DentrySpellingCache::new(wt);
        spellings.prefetch_parent_dirs(paths.iter().map(String::as_str), NonZeroUsize::MIN);
        assert_eq!(
            spellings.listings.len(),
            5,
            "exactly root, a, a/b, a/c and Top are read: {:?}",
            spellings.listings.keys().collect::<Vec<_>>()
        );
        for query in &queries {
            assert_eq!(
                spellings.spelling(query).map(Cow::into_owned),
                worktree_dentry_spelling(wt, query),
                "{query}"
            );
        }
        for path in &paths {
            assert!(
                matches!(spellings.spelling(path), Some(Cow::Borrowed(p)) if p == path),
                "{path} exists exactly as spelled"
            );
        }
        assert_eq!(spellings.spelling("A/B/TWO").as_deref(), Some("a/b/Two"));
        assert_eq!(spellings.spelling("a/b/missing"), None);
        assert_eq!(spellings.spelling(""), None);
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
        let mut cache = DentrySpellingCache::new(wt);
        for i in 0..n {
            let rel = format!("d/f{i:05}.txt");
            assert_eq!(
                cache.spelling_with(&rel, &mut counting).as_deref(),
                Some(rel.as_str())
            );
        }
        assert_eq!(
            cache
                .spelling_with("D/SUB/leaf.TXT", &mut counting)
                .as_deref(),
            Some("d/sub/Leaf.txt")
        );
        assert_eq!(cache.spelling_with("d/missing.txt", &mut counting), None);
        assert_eq!(cache.spelling_with("nope/x", &mut counting), None);
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
        let mut cache = DentrySpellingCache::new(wt);
        assert_eq!(cache.spelling("b/x").as_deref(), Some("b/x"));
        assert_eq!(cache.spelling("B/x").as_deref(), Some("B/x"));
        assert_eq!(cache.spelling("cc/x").as_deref(), Some("Cc/x"));
        assert_eq!(cache.spelling("Empty/y").as_deref(), Some("empty/y"));
        assert_eq!(cache.spelling("b/X").as_deref(), Some("b/x"));
        assert_eq!(cache.spelling("b/z"), None);
    }

    #[test]
    fn dentry_spellings_prefer_exact_component_then_backtrack() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        if !worktree_has_distinct_icase_dentries(wt) {
            return;
        }
        std::fs::create_dir_all(wt.join("Dir")).expect("Dir");
        std::fs::create_dir_all(wt.join("dir")).expect("dir");
        std::fs::write(wt.join("Dir/x"), b"x").expect("write");
        std::fs::write(wt.join("dir/x"), b"x").expect("write");
        std::fs::write(wt.join("dir/only-lower"), b"x").expect("write");
        let mut spellings = DentrySpellingCache::new(wt);
        assert_eq!(spellings.spelling("dir/x").as_deref(), Some("dir/x"));
        assert_eq!(spellings.spelling("Dir/x").as_deref(), Some("Dir/x"));
        assert_eq!(spellings.spelling("DIR/x").as_deref(), Some("Dir/x"));
        assert_eq!(
            spellings.spelling("DIR/ONLY-LOWER").as_deref(),
            Some("dir/only-lower")
        );
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
    fn directory_collision_winner_skips_sibling_worktree_bytes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::create_dir_all(wt.join("Docs")).expect("mkdir");
        std::fs::write(wt.join("Docs/guide.md"), b"two\n").expect("write");
        let one = ObjectId::from_hex("1111111111111111111111111111111111111111").unwrap();
        let two = ObjectId::from_hex("2222222222222222222222222222222222222222").unwrap();
        let mut index = Index::new();
        let mut upper = dummy_entry("Docs/guide.md");
        upper.oid = one;
        let mut lower = dummy_entry("docs/guide.md");
        lower.oid = two;
        index.push_entry_unsorted(upper);
        index.push_entry_unsorted(lower);
        index.sort();
        let map = IcaseCheckoutCollisionMap::from_index(wt, &index, true);
        assert!(!map.is_auto_stage_loser(b"Docs/guide.md"));
        assert!(map.is_auto_stage_loser(b"docs/guide.md"));
        assert!(map.worktree_oid_is_collision_sibling_blob(b"Docs/guide.md", two, one));
        assert!(!map.worktree_oid_is_collision_sibling_blob(b"Docs/guide.md", one, one));
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
