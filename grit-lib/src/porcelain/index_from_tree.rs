//! Build stage-0 indexes from tree objects and refresh worktree stat caches.

use std::collections::BTreeSet;
use std::path::Path;

use crate::diff::refresh_index_stat_content_verified_with_rules;
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, CacheTreeNode, Index, IndexEntry, MODE_TREE};
use crate::objects::{parse_tree, ObjectId, ObjectKind};
use crate::path_population::validate_tree_walk_entry;
use crate::path_validate::ProtectOptions;
use crate::porcelain::merge::tree_to_index_entries;
use crate::repo::Repository;
use crate::repo_relpath::join_rel;
use crate::sparse_checkout::SparseCheckout;
use crate::worktree_rules::WorktreeRules;

/// Build a sorted stage-0 index whose OIDs and modes match `tree_oid` (zeroed stat fields).
pub(crate) fn index_from_tree(repo: &Repository, tree_oid: &ObjectId) -> Result<Index> {
    let entries = tree_to_index_entries(repo, tree_oid, "")?;
    let mut index = Index::new();
    for entry in entries {
        index.push_entry_unsorted(entry);
    }
    index.sort();
    Ok(index)
}

/// Refresh stage-0 stat fields from the worktree when blob content matches the index OID.
///
/// Tree-built indexes start with zeroed stat data; callers must run this before writing the
/// index when the worktree still matches the tree (mixed reset, stash push, merge abort).
pub(crate) fn refresh_stage0_index_stats_from_worktree(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
) -> Result<()> {
    if !index.entries.iter().any(|e| e.stage() == 0) {
        return Ok(());
    }
    let index_mtime = index_file_mtime(&repo.index_path());
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    let _ = refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        index,
        work_tree,
        index_mtime,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )?;
    Ok(())
}

/// Like [`refresh_stage0_index_stats_from_worktree`], limited to the stage-0 entries of `paths`.
///
/// For callers that just wrote those files (restore, partial restore, history undo): an entry
/// whose cleaned worktree content (autocrlf, eol, ident, filters) matches its OID takes the
/// file's stat data; any other entry keeps the stat fields it had.
///
/// Returns whether any entry changed, so a caller that has nothing else to write can leave the
/// index file alone.
pub(crate) fn refresh_stage0_index_stats_for_paths(
    repo: &Repository,
    work_tree: &Path,
    index: &mut Index,
    paths: &BTreeSet<&str>,
) -> Result<bool> {
    let entries: Vec<_> = paths
        .iter()
        .filter_map(|path| index.get(path.as_bytes(), 0))
        .cloned()
        .collect();
    if entries.is_empty() {
        return Ok(false);
    }
    let mut subset = Index {
        version: index.version,
        hash_algo: index.hash_algo,
        source_mtime: index.source_mtime,
        source_checksum: index.source_checksum.clone(),
        source_file_len: index.source_file_len,
        ..Index::new()
    };
    subset.entries = entries;
    let index_mtime = index_file_mtime(&repo.index_path());
    let rules = WorktreeRules::from_repository(repo, index).ok();
    let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
    let changed = refresh_index_stat_content_verified_with_rules(
        repo.odb(),
        repo.git_dir(),
        &mut subset,
        work_tree,
        index_mtime,
        repo.config().ok().as_deref(),
        None,
        rules_arc.as_ref(),
    )?;
    if changed {
        for entry in subset.entries {
            index.stage_file(entry);
        }
    }
    Ok(changed)
}

/// Like [`index_from_tree`], then refresh stat caches from the worktree when a work tree exists.
///
/// In a sparse checkout the rebuilt index keeps the on-disk index's `skip-worktree` bits (see
/// [`SparseCheckout::carry_skip_worktree`]).
pub(crate) fn index_from_tree_with_worktree_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
) -> Result<Index> {
    let mut index = index_from_tree(repo, tree_oid)?;
    if let Some(sparse) = SparseCheckout::load(repo)? {
        sparse.carry_skip_worktree(&mut index, &repo.load_index()?);
    }
    if let Some(work_tree) = repo.work_tree() {
        refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    }
    Ok(index)
}

/// Like [`index_from_tree_with_worktree_stats`], but first copies cached stat data from `seed`
/// (normally the repository's on-disk index) for entries with the same path, mode and blob id,
/// and primes the cache-tree from `tree_oid`.
///
/// The worktree refresh then only hashes files whose stat no longer matches, instead of every
/// file in the tree. Racy seeded entries are still re-hashed because the refresh compares against
/// the on-disk index's mtime, which is where `seed` stat data was recorded. With the primed
/// cache-tree, writing a tree after staging a few paths rebuilds only the directories those paths
/// invalidate.
pub(crate) fn index_from_tree_with_seeded_worktree_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
    seed: &Index,
) -> Result<Index> {
    let mut index = index_from_tree_with_seeded_stats(repo, tree_oid, seed)?;
    if let Some(work_tree) = repo.work_tree() {
        refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    }
    Ok(index)
}

/// Like [`index_from_tree_with_seeded_worktree_stats`], consuming `seed`.
///
/// When `seed`'s cache-tree root already records `tree_oid` (for example `reset` to the commit
/// the index was last written from), its rows are normalized in place instead of being copied,
/// re-sorted and merged; the result is the same index either way.
pub(crate) fn index_from_tree_with_owned_seed_worktree_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
    mut seed: Index,
) -> Result<Index> {
    let keep_skip_worktree = SparseCheckout::load(repo)?.is_some();
    let mut index = match index_from_matching_seed(&mut seed, tree_oid, keep_skip_worktree) {
        Some(index) => index,
        None => index_from_tree_with_seeded_stats(repo, tree_oid, &seed)?,
    };
    drop(seed);
    if let Some(work_tree) = repo.work_tree() {
        refresh_stage0_index_stats_from_worktree(repo, work_tree, &mut index)?;
    }
    Ok(index)
}

/// Move `seed`'s rows into the index [`index_from_tree_with_seeded_stats`] would build for
/// `tree_oid` when `seed`'s valid cache-tree root names that tree and covers every row (leaving
/// `seed` without rows or cache-tree); otherwise return `None` and leave `seed` untouched.
///
/// Each row keeps its path, mode and oid, and its stat data under the same conditions
/// [`seed_stage0_stat_data`] copies it; flags are reset as a tree read leaves them, except that
/// `keep_skip_worktree` (a sparse checkout) carries the skip-worktree bit over.
fn index_from_matching_seed(
    seed: &mut Index,
    tree_oid: &ObjectId,
    keep_skip_worktree: bool,
) -> Option<Index> {
    let matches = seed
        .cache_tree
        .as_ref()
        .filter(|root| root.entry_count >= 0 && root.oid == Some(*tree_oid))
        .is_some_and(|root| seed_rows_under(&seed.entries, b"", root.entry_count).is_some());
    if !matches {
        return None;
    }
    let cache_tree = seed.cache_tree.take()?;
    let mut index = Index::new();
    index.entries = std::mem::take(&mut seed.entries);
    for entry in &mut index.entries {
        let keep_stat = seed_row_stat_reusable(entry);
        let skip_worktree = keep_skip_worktree && entry.skip_worktree();
        let mut row = tree_entry(std::mem::take(&mut entry.path), entry.mode, entry.oid);
        if keep_stat {
            copy_stat(&mut row, entry);
        }
        if skip_worktree {
            row.set_skip_worktree(true);
        }
        *entry = row;
    }
    index.mark_entries_unsorted();
    index.sort();
    index.set_cache_tree(cache_tree);
    Some(index)
}

/// Like [`index_from_tree`], but copies cached stat data from `seed` for entries with the same
/// path, mode and blob id, and primes the cache-tree from `tree_oid`. Does not touch the worktree.
///
/// Stat data copied from `seed` is only trustworthy relative to the mtime of the index file
/// `seed` was loaded from; callers comparing the result against the worktree must judge
/// raciness against that mtime.
pub(crate) fn index_from_tree_with_seeded_stats(
    repo: &Repository,
    tree_oid: &ObjectId,
    seed: &Index,
) -> Result<Index> {
    let mut index = index_from_tree_reusing_cache_tree(repo, tree_oid, seed)?;
    if let Some(sparse) = SparseCheckout::load(repo)? {
        sparse.carry_skip_worktree(&mut index, seed);
    }
    seed_stage0_stat_data(&mut index, seed);
    Ok(index)
}

/// Same entries and cache-tree as [`index_from_tree`] followed by
/// [`crate::write_tree::build_cache_tree_from_tree`], but subtrees whose `seed` cache-tree node is valid and
/// records the same tree id are taken from `seed`'s rows instead of being read from the object
/// database. Rows taken from `seed` carry only path, mode and oid (stat data and flags are reset
/// exactly as a tree read would leave them). Matching sparse-directory rows stay opaque and
/// retain skip-worktree; their cache-tree nodes count one index row. The cache-tree is attached
/// only when its entry count matches the rows (duplicate tree entries can yield more entries).
pub(crate) fn index_from_tree_reusing_cache_tree(
    repo: &Repository,
    tree_oid: &ObjectId,
    seed: &Index,
) -> Result<Index> {
    let seed_root = seed.cache_tree.as_ref();
    let mut walk = TreeIndexWalk {
        repo,
        protect: ProtectOptions::from_config(&*repo.config()?),
        seed: &seed.entries,
        entries: Vec::with_capacity(seed.entries.len()),
    };
    let cache_tree = walk.walk(tree_oid, b"", Vec::new(), seed_root)?;
    let mut index = Index::new();
    index.entries = walk.entries;
    index.mark_entries_unsorted();
    index.sort();
    index.sparse_directories = index.has_sparse_directory_placeholders();
    if usize::try_from(cache_tree.entry_count).ok() == Some(index.entries.len()) {
        index.set_cache_tree(cache_tree);
    }
    Ok(index)
}

/// Load the seed of a hard reset to `target`.
///
/// Absent excluded directories stay opaque whenever `target` still has a tree at their path, even
/// a different one: a hard reset discards staged content and never writes excluded paths, so the
/// sparse-directory row simply takes the target subtree id (as Git's sparse-index reset does).
/// Retargeted rows drop their cache-tree node. Everything else is prepared as in
/// [`prepare_index_for_tree_transition`].
pub(crate) fn load_index_for_hard_reset(repo: &Repository, target: &ObjectId) -> Result<Index> {
    let mut index = repo.load_index_preserving_sparse()?;
    prepare_sparse_rows(repo, target, &mut index, true)?;
    Ok(index)
}

/// Prepare loaded rows once the transition's target tree has been resolved, keeping matching
/// absent excluded directories opaque. Changed trees, directories present on disk, and
/// definitions that no longer exclude a directory use the ordinary expanded representation so
/// overwrite guards and staged-content checks stay precise.
pub(crate) fn prepare_index_for_tree_transition(
    repo: &Repository,
    target: &ObjectId,
    index: &mut Index,
) -> Result<()> {
    prepare_sparse_rows(repo, target, index, false)
}

/// Shared body of [`prepare_index_for_tree_transition`] and [`load_index_for_hard_reset`];
/// `retarget` keeps candidates whose target subtree differs and points them at it.
fn prepare_sparse_rows(
    repo: &Repository,
    target: &ObjectId,
    index: &mut Index,
    retarget: bool,
) -> Result<()> {
    let config = repo.config()?;
    if !index.has_sparse_directory_placeholders() {
        repo.clear_present_skip_worktree(index, config.as_ref());
        return Ok(());
    }
    let protect = ProtectOptions::from_config(config.as_ref());
    let sparse = SparseCheckout::load(repo)?;
    let worktree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let index_sparse = config
        .get_bool("index.sparse")
        .and_then(|v| v.ok())
        .unwrap_or(false);
    let mut candidates = std::collections::BTreeMap::new();
    for row in &index.entries {
        if row.is_sparse_directory_placeholder()
            && index_sparse
            && sparse
                .as_ref()
                .is_some_and(|s| s.excludes_directory(&row.path))
            && std::fs::symlink_metadata(crate::index::worktree_path_from_index_rel(
                worktree,
                row.path.strip_suffix(b"/").unwrap_or(&row.path),
            ))
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
        {
            candidates.insert(row.path.clone(), row.oid);
        }
    }
    let mut keep = std::collections::BTreeMap::new();
    if !candidates.is_empty() {
        matching_sparse_trees(repo, target, b"", &candidates, retarget, &mut keep, protect)?;
    }
    index.expand_sparse_directories_matching(repo.odb(), protect, |row| {
        !keep.contains_key(&row.path)
    })?;
    let mut retargeted = Vec::new();
    for row in &mut index.entries {
        let Some(&oid) = keep.get(&row.path) else {
            continue;
        };
        if row.is_sparse_directory_placeholder() && row.oid != oid {
            row.oid = oid;
            retargeted.push(row.path.clone());
        }
    }
    for path in retargeted {
        index.invalidate_cache_tree_for_path(path.strip_suffix(b"/").unwrap_or(&path));
    }
    repo.clear_present_skip_worktree(index, config.as_ref());
    Ok(())
}

/// Walk `tree` (at directory `prefix`, no trailing slash) and record in `keep` the sparse-directory
/// `candidates` that stay opaque, with the target subtree id each must record: identical subtrees,
/// or any subtree when `retarget` is set. Descends only into directories holding candidates.
fn matching_sparse_trees(
    repo: &Repository,
    tree: &ObjectId,
    prefix: &[u8],
    candidates: &std::collections::BTreeMap<Vec<u8>, ObjectId>,
    retarget: bool,
    keep: &mut std::collections::BTreeMap<Vec<u8>, ObjectId>,
    protect: ProtectOptions,
) -> Result<()> {
    let obj = repo.odb().read(tree)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::UnexpectedObjectKind {
            oid: *tree,
            expected: ObjectKind::Tree,
            actual: obj.kind,
        });
    }
    for entry in parse_tree(&obj.data)? {
        validate_tree_walk_entry(prefix, &entry.name, entry.mode, protect)?;
        if entry.mode != MODE_TREE {
            continue;
        }
        let mut path = join_rel(prefix, &entry.name);
        path.push(b'/');
        if let Some(oid) = candidates.get(&path) {
            if retarget || *oid == entry.oid {
                keep.insert(path, entry.oid);
            }
        } else if candidates
            .range(path.clone()..)
            .next()
            .is_some_and(|(key, _)| key.starts_with(&path))
        {
            path.pop();
            matching_sparse_trees(repo, &entry.oid, &path, candidates, retarget, keep, protect)?;
        }
    }
    Ok(())
}

/// Expose rows needed by a tree delta, including a directory/file replacement at its root.
pub(crate) fn expand_sparse_for_tree_changes(
    repo: &Repository,
    index: &mut Index,
    changes: &[crate::diff::DiffEntry],
) -> Result<()> {
    if !index.has_sparse_directory_placeholders() {
        return Ok(());
    }
    let paths: Vec<Vec<u8>> = changes
        .iter()
        .flat_map(|change| {
            [change.old_path.as_deref(), change.new_path.as_deref()]
                .into_iter()
                .flatten()
        })
        .map(|path| crate::repo_relpath::storage_bytes(path).into_owned())
        .collect();
    index.expand_sparse_directories_matching(
        repo.odb(),
        ProtectOptions::from_config(&*repo.config()?),
        |row| {
            let dir = row.path.strip_suffix(b"/").unwrap_or(&row.path);
            paths.iter().any(|path| {
                path.starts_with(&row.path)
                    || path == dir
                    || dir
                        .strip_prefix(path.as_slice())
                        .is_some_and(|tail| tail.starts_with(b"/"))
            })
        },
    )
}

/// Depth-first tree flattening for [`index_from_tree_reusing_cache_tree`].
struct TreeIndexWalk<'a> {
    repo: &'a Repository,
    protect: ProtectOptions,
    /// Rows of the seed index in canonical order.
    seed: &'a [IndexEntry],
    entries: Vec<IndexEntry>,
}

impl TreeIndexWalk<'_> {
    /// Append the entries of `tree_oid` (at directory `prefix`, no trailing slash) and return its
    /// cache-tree node named `name`.
    fn walk(
        &mut self,
        tree_oid: &ObjectId,
        prefix: &[u8],
        name: Vec<u8>,
        seed_node: Option<&CacheTreeNode>,
    ) -> Result<CacheTreeNode> {
        if !prefix.is_empty() {
            let mut path = prefix.to_vec();
            path.push(b'/');
            let pos = self.seed.partition_point(|row| row.path < path);
            if let Some(row) = self.seed.get(pos).filter(|row| {
                row.path == path && row.is_sparse_directory_placeholder() && row.oid == *tree_oid
            }) {
                self.entries.push(row.clone());
                return Ok(CacheTreeNode::valid(name, 1, *tree_oid, Vec::new()));
            }
        }
        if let Some(node) = seed_node.filter(|n| n.entry_count >= 0 && n.oid == Some(*tree_oid)) {
            if let Some(rows) = seed_rows_under(self.seed, prefix, node.entry_count) {
                self.entries.extend(
                    rows.iter()
                        .map(|row| tree_entry(row.path.clone(), row.mode, row.oid)),
                );
                let mut reused = node.clone();
                reused.name = name;
                return Ok(reused);
            }
        }

        let obj = self.repo.odb().read(tree_oid)?;
        if obj.kind != ObjectKind::Tree {
            return Err(Error::UnexpectedObjectKind {
                oid: *tree_oid,
                expected: ObjectKind::Tree,
                actual: obj.kind,
            });
        }
        let mut entry_count: i32 = 0;
        let mut children = Vec::new();
        for te in parse_tree(&obj.data)? {
            validate_tree_walk_entry(prefix, &te.name, te.mode, self.protect)?;
            let path = join_rel(prefix, &te.name);
            if te.mode == MODE_TREE {
                let child_seed = seed_node.and_then(|n| find_child(n, &te.name));
                let child = self.walk(&te.oid, &path, te.name, child_seed)?;
                entry_count = entry_count.saturating_add(child.entry_count.max(0));
                children.push(child);
            } else {
                self.entries.push(tree_entry(path, te.mode, te.oid));
                entry_count = entry_count.saturating_add(1);
            }
        }
        children.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(CacheTreeNode::valid(name, entry_count, *tree_oid, children))
    }
}

/// Stage-0 entry with zeroed stat data, as read from a tree.
fn tree_entry(path: Vec<u8>, mode: u32, oid: ObjectId) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path,
        base_index_pos: 0,
    }
}

/// Child cache-tree node called `name`. Git orders children by name length first, so the list
/// is searched rather than bisected.
fn find_child<'a>(node: &'a CacheTreeNode, name: &[u8]) -> Option<&'a CacheTreeNode> {
    node.children.iter().find(|c| c.name == name)
}

/// The `count` rows of `seed` under directory `prefix` (empty for the root), when they form
/// exactly the plain stage-0 rows of that directory: the cache-tree count must cover every row
/// under the prefix and nothing else, so a stale node is never trusted.
fn seed_rows_under<'a>(
    seed: &'a [IndexEntry],
    prefix: &[u8],
    count: i32,
) -> Option<&'a [IndexEntry]> {
    let count = usize::try_from(count).ok()?;
    let dir = if prefix.is_empty() {
        Vec::new()
    } else {
        let mut dir = prefix.to_vec();
        dir.push(b'/');
        dir
    };
    let start = seed.partition_point(|e| e.path.as_slice() < dir.as_slice());
    let end = start.checked_add(count)?;
    let rows = seed.get(start..end)?;
    let plain = rows.iter().all(|e| {
        e.path.starts_with(&dir) && e.stage() == 0 && !e.intent_to_add() && e.mode != MODE_TREE
    });
    let closed = seed.get(end).is_none_or(|e| !e.path.starts_with(&dir));
    (plain && closed).then_some(rows)
}

/// Copy stat fields from `seed` into `index` entries whose path, mode and oid match a clean
/// stage-0 seed entry. Both indexes must be in canonical `(path, stage)` order; entries that are
/// not found keep their zeroed stat data.
fn seed_stage0_stat_data(index: &mut Index, seed: &Index) {
    let mut seeds = seed
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && seed_row_stat_reusable(e))
        .peekable();
    for entry in index.entries.iter_mut().filter(|e| e.stage() == 0) {
        while seeds.next_if(|s| s.path < entry.path).is_some() {}
        let Some(src) = seeds.next_if(|s| s.path == entry.path) else {
            continue;
        };
        if src.oid != entry.oid || src.mode != entry.mode {
            continue;
        }
        copy_stat(entry, src);
    }
}

/// Whether a seed row's stat data describes its work-tree file and may be carried over.
fn seed_row_stat_reusable(entry: &IndexEntry) -> bool {
    !entry.stat_cache_unset()
        && !entry.skip_worktree()
        && !entry.assume_unchanged()
        && !entry.intent_to_add()
}

/// Copy the cached stat fields of `src` into `dst`.
fn copy_stat(dst: &mut IndexEntry, src: &IndexEntry) {
    dst.ctime_sec = src.ctime_sec;
    dst.ctime_nsec = src.ctime_nsec;
    dst.mtime_sec = src.mtime_sec;
    dst.mtime_nsec = src.mtime_nsec;
    dst.dev = src.dev;
    dst.ino = src.ino;
    dst.uid = src.uid;
    dst.gid = src.gid;
    dst.size = src.size;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::index::{IndexEntry, MODE_EXECUTABLE, MODE_REGULAR};
    use crate::objects::ObjectKind;
    use crate::porcelain::add::{stage, StageOptions};
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::state::resolve_head_for_repository;
    use crate::write_tree::cache_tree_fully_valid;
    use std::fs;
    use tempfile::TempDir;

    /// `mtime == 0` builds a tree-style entry with every stat field zeroed.
    fn entry(path: &str, oid: ObjectId, mode: u32, mtime: u32) -> IndexEntry {
        let stat = |v: u32| if mtime == 0 { 0 } else { v };
        IndexEntry {
            ctime_sec: mtime,
            ctime_nsec: stat(7),
            mtime_sec: mtime,
            mtime_nsec: stat(9),
            dev: stat(1),
            ino: mtime,
            mode,
            uid: stat(2),
            gid: stat(3),
            size: stat(4),
            oid,
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    fn index_of(entries: Vec<IndexEntry>) -> Index {
        let mut index = Index::new();
        for e in entries {
            index.push_entry_unsorted(e);
        }
        index.sort();
        index
    }

    #[test]
    fn seed_copies_stat_only_for_same_oid_and_mode() {
        let a = ObjectId::from_bytes(&[1; 20]).expect("oid");
        let b = ObjectId::from_bytes(&[2; 20]).expect("oid");
        let mut target = index_of(vec![
            entry("same", a, MODE_REGULAR, 0),
            entry("other-oid", a, MODE_REGULAR, 0),
            entry("other-mode", a, MODE_REGULAR, 0),
            entry("only-in-tree", a, MODE_REGULAR, 0),
            entry("skipped", a, MODE_REGULAR, 0),
        ]);
        let mut skipped = entry("skipped", a, MODE_REGULAR, 50);
        skipped.flags |= 0x8000;
        let seed = index_of(vec![
            entry("same", a, MODE_REGULAR, 10),
            entry("other-oid", b, MODE_REGULAR, 20),
            entry("other-mode", a, MODE_EXECUTABLE, 30),
            entry("only-in-seed", a, MODE_REGULAR, 40),
            skipped,
        ]);
        seed_stage0_stat_data(&mut target, &seed);

        let same = target.get(b"same", 0).expect("same");
        assert_eq!((same.mtime_sec, same.ino, same.size), (10, 10, 4));
        for path in ["other-oid", "other-mode", "only-in-tree", "skipped"] {
            let e = target.get(path.as_bytes(), 0).expect(path);
            assert!(e.stat_cache_unset(), "{path} must keep zeroed stat");
        }
    }

    #[test]
    fn seeded_index_matches_tree_and_primes_valid_cache_tree() {
        let dir = TempDir::new().expect("tempdir");
        let root = dir.path();
        crate::repo::init_repository(root, false, "main", None, crate::RefStorageFormat::Files)
            .expect("init");
        for rel in ["a/x.txt", "a/b/y.txt", "z.txt"] {
            let abs = root.join(rel);
            fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
            fs::write(abs, rel.as_bytes()).expect("write");
        }
        let repo = Repository::discover(Some(root), &RepositoryOptions::empty()).expect("repo");
        stage(
            &repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_owned();
        create_commit(
            &repo,
            &CommitRequest {
                message: "init".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                ..Default::default()
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("commit");
        let head = resolve_head_for_repository(&repo).expect("head");
        let commit = repo.odb().read(head.oid().expect("oid")).expect("read");
        let tree = crate::objects::parse_commit(&commit.data)
            .expect("parse")
            .tree;
        fs::write(root.join("z.txt"), b"changed").expect("modify");

        let real = repo.load_index().expect("index");
        let seeded =
            index_from_tree_with_seeded_worktree_stats(&repo, &tree, &real).expect("seeded");
        let plain = index_from_tree_with_worktree_stats(&repo, &tree).expect("plain");

        let oids = |idx: &Index| -> Vec<(Vec<u8>, ObjectId, u32)> {
            idx.entries
                .iter()
                .map(|e| (e.path.clone(), e.oid, e.mode))
                .collect()
        };
        assert_eq!(oids(&seeded), oids(&plain));
        assert!(cache_tree_fully_valid(
            repo.odb(),
            seeded.cache_tree.as_ref()
        ));
        assert_eq!(seeded.cache_tree_root, Some(tree));
        assert!(!seeded.get(b"a/x.txt", 0).expect("x").stat_cache_unset());
        assert_eq!(
            seeded.get(b"z.txt", 0).expect("z").oid,
            repo.odb().write(ObjectKind::Blob, b"z.txt").expect("blob")
        );
    }

    /// Write `files`, stage everything and commit; returns the new root tree.
    fn commit_files(repo: &Repository, files: &[(&str, &str)]) -> ObjectId {
        let root = repo.work_tree().expect("work tree");
        for (rel, contents) in files {
            let abs = root.join(rel);
            fs::create_dir_all(abs.parent().expect("parent")).expect("mkdir");
            fs::write(abs, contents).expect("write");
        }
        stage(
            repo,
            &StageOptions::default(),
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_owned();
        create_commit(
            repo,
            &CommitRequest {
                message: "c".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                ..Default::default()
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("commit");
        let head = resolve_head_for_repository(repo).expect("head");
        let commit = repo.odb().read(head.oid().expect("oid")).expect("read");
        crate::objects::parse_commit(&commit.data)
            .expect("parse")
            .tree
    }

    #[test]
    fn path_refresh_reports_whether_an_entry_changed() {
        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("repo");
        commit_files(&repo, &[("a.txt", "one\n"), ("b.txt", "two\n")]);
        let work_tree = repo.work_tree().expect("work tree").to_path_buf();
        let mut index = repo.load_index().expect("index");

        let mut stale = index.get(b"a.txt", 0).expect("a.txt").clone();
        stale.mtime_sec = 1;
        stale.mtime_nsec = 0;
        index.stage_file(stale);
        fs::write(work_tree.join("b.txt"), "changed\n").expect("modify");
        let b_before = index.get(b"b.txt", 0).expect("b.txt").clone();

        let modified = BTreeSet::from(["b.txt", "missing.txt"]);
        assert!(
            !refresh_stage0_index_stats_for_paths(&repo, &work_tree, &mut index, &modified)
                .expect("refresh")
        );
        assert_eq!(index.get(b"b.txt", 0), Some(&b_before));

        let unchanged = BTreeSet::from(["a.txt"]);
        assert!(
            refresh_stage0_index_stats_for_paths(&repo, &work_tree, &mut index, &unchanged)
                .expect("refresh")
        );
        let meta = fs::metadata(work_tree.join("a.txt")).expect("stat");
        let mtime = meta
            .modified()
            .expect("mtime")
            .duration_since(std::time::UNIX_EPOCH)
            .expect("epoch")
            .as_secs();
        assert_eq!(
            u64::from(index.get(b"a.txt", 0).expect("a.txt").mtime_sec),
            mtime
        );
        assert!(
            !refresh_stage0_index_stats_for_paths(&repo, &work_tree, &mut index, &unchanged)
                .expect("refresh"),
            "an entry that already holds the file's stat is left alone"
        );
    }

    /// Repository with commit `t1` (`a/x`, `a/b/y`, `c/z`, `top`) followed by commit `t2`, which
    /// only changes `c/z`. The index is left at `t2`.
    fn two_commit_repo() -> (TempDir, Repository, ObjectId, ObjectId) {
        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("repo");
        let t1 = commit_files(
            &repo,
            &[
                ("a/x", "x\n"),
                ("a/b/y", "y\n"),
                ("c/z", "z\n"),
                ("top", "t\n"),
            ],
        );
        let t2 = commit_files(&repo, &[("c/z", "z2\n")]);
        (dir, repo, t1, t2)
    }

    /// What reading `tree` from scratch yields: flattened entries plus the tree's cache-tree.
    fn plain_tree_index(repo: &Repository, tree: &ObjectId) -> Index {
        let mut index = index_from_tree(repo, tree).expect("tree index");
        index.set_cache_tree(
            crate::write_tree::build_cache_tree_from_tree(repo.odb(), tree).expect("cache-tree"),
        );
        index
    }

    /// Cache-tree with children in name order; Git does not order them that way on disk.
    fn normalized(node: &CacheTreeNode) -> CacheTreeNode {
        let mut children: Vec<CacheTreeNode> = node.children.iter().map(normalized).collect();
        children.sort_by(|a, b| a.name.cmp(&b.name));
        CacheTreeNode {
            children,
            ..node.clone()
        }
    }

    fn assert_same_index(actual: &Index, expected: &Index) {
        assert_eq!(actual.entries, expected.entries);
        assert_eq!(actual.cache_tree_root, expected.cache_tree_root);
        assert_eq!(
            actual.cache_tree.as_ref().map(normalized),
            expected.cache_tree.as_ref().map(normalized)
        );
    }

    fn loose_object_path(repo: &Repository, oid: &ObjectId) -> std::path::PathBuf {
        let hex = oid.to_hex();
        repo.git_dir()
            .join("objects")
            .join(&hex[..2])
            .join(&hex[2..])
    }

    fn subtree_oid(repo: &Repository, tree: &ObjectId, name: &[u8]) -> ObjectId {
        let obj = repo.odb().read(tree).expect("tree");
        parse_tree(&obj.data)
            .expect("parse")
            .into_iter()
            .find(|e| e.name == name)
            .expect("subtree")
            .oid
    }

    #[test]
    fn reusing_walk_matches_tree_read_for_full_partial_and_missing_seed() {
        let (_dir, repo, t1, t2) = two_commit_repo();
        let at_t2 = repo.load_index().expect("index");
        assert_eq!(
            at_t2.cache_tree_root,
            Some(t2),
            "commit leaves a valid cache-tree"
        );

        for (target, seed) in [(t2, &at_t2), (t1, &at_t2), (t1, &Index::new())] {
            let reused = index_from_tree_reusing_cache_tree(&repo, &target, seed).expect("walk");
            assert_same_index(&reused, &plain_tree_index(&repo, &target));
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn sparse_reset_and_switch_do_not_expand_unchanged_monorepo_directories() {
        use crate::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
        use crate::porcelain::switch::{switch_repository, SwitchMode, SwitchOptions};
        use crate::progress::NeverCancel;
        for paths in [120_000, 240_000] {
            let dir = TempDir::new().expect("tempdir");
            grit_test_support::sparse_scale::create(Path::new("git"), dir.path(), paths, false, 0)
                .expect("sparse fixture");
            let mut env = crate::environment::Environment::empty();
            env.cwd = dir.path().to_path_buf();
            let repo =
                Repository::discover(Some(dir.path()), &RepositoryOptions::with_environment(env))
                    .expect("repo");
            let resolve = |name: &str| {
                grit_test_support::git(dir.path(), &["rev-parse", name])
                    .trim()
                    .parse::<ObjectId>()
                    .expect("oid")
            };
            let base = resolve("sparse-base");
            let edited = resolve("sparse-edited");
            for target in [base, edited] {
                reset_to_commit(
                    &repo,
                    &ResetOptions::new(
                        target,
                        ResetMode::Hard,
                        "T <t@example.com> 1700000020 +0000"
                            .parse()
                            .expect("signature"),
                        "reset",
                    ),
                    &mut NullProgress,
                    &NeverCancel,
                )
                .expect("reset");
                let index = Index::load(&repo.index_path()).expect("raw index");
                assert_eq!(index.entries.len(), 700);
                assert_eq!(
                    index
                        .entries
                        .iter()
                        .filter(|e| e.is_sparse_directory_placeholder())
                        .count(),
                    400
                );
                assert_eq!(repo.odb().hot_path_metrics().sparse_expanded_rows(), 0);
            }
            for target in [base, edited] {
                switch_repository(
                    &repo,
                    &SwitchOptions {
                        mode: SwitchMode::Detached {
                            commit: target,
                            reflog_to_label: target.to_string(),
                        },
                        dry_run: false,
                        hooks: crate::PostOperationHooks::Skip,
                    },
                    &mut NullProgress,
                    &NeverCancel,
                )
                .expect("switch");
                assert_eq!(repo.odb().hot_path_metrics().sparse_expanded_rows(), 0);
                let index = Index::load(&repo.index_path()).expect("raw index");
                assert_eq!(index.entries.len(), 700);
                crate::write_tree::verify_cache_tree(&index).expect("sparse cache-tree");
                assert_eq!(
                    grit_test_support::git(dir.path(), &["write-tree"]).trim(),
                    resolve("HEAD^{tree}").to_string()
                );
                assert!(grit_test_support::git(dir.path(), &["status", "--porcelain"]).is_empty());
            }
            grit_test_support::git(dir.path(), &["fsck", "--strict"]);
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn sparse_hard_reset_retargets_changed_excluded_directories_git_compat() {
        use crate::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
        use crate::progress::NeverCancel;
        let dir = TempDir::new().expect("tempdir");
        // Every one of the 400 excluded directories changes between the two commits.
        grit_test_support::sparse_scale::create(Path::new("git"), dir.path(), 12_000, true, 2_000)
            .expect("sparse fixture");
        let git = |args: &[&str]| grit_test_support::git(dir.path(), args);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("repo");
        for name in ["sparse-base", "sparse-edited", "sparse-base"] {
            let commit: ObjectId = git(&["rev-parse", name]).trim().parse().expect("oid");
            reset_to_commit(
                &repo,
                &ResetOptions::new(
                    commit,
                    ResetMode::Hard,
                    "T <t@example.com> 1700000020 +0000"
                        .parse()
                        .expect("signature"),
                    "reset",
                ),
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("reset");
            assert_eq!(repo.odb().hot_path_metrics().sparse_expanded_rows(), 0);
            let index = Index::load(&repo.index_path()).expect("raw index");
            assert_eq!(index.entries.len(), 700);
            crate::write_tree::verify_cache_tree(&index).expect("sparse cache-tree");
            let sparse_rows = git(&["ls-files", "--sparse", "-s"]);
            let placeholders: Vec<&str> = sparse_rows
                .lines()
                .filter(|l| l.starts_with("040000 "))
                .collect();
            assert_eq!(placeholders.len(), 400);
            for row in placeholders {
                let (meta, path) = row.split_once('\t').expect("row");
                let oid = meta.split(' ').nth(1).expect("oid");
                let dir_name = path.trim_end_matches('/');
                assert_eq!(
                    git(&["rev-parse", &format!("{name}:{dir_name}")]).trim(),
                    oid,
                    "{path} must record the target subtree"
                );
            }
            assert_eq!(
                git(&["write-tree"]).trim(),
                git(&["rev-parse", &format!("{name}^{{tree}}")]).trim()
            );
            assert!(git(&["status", "--porcelain"]).is_empty());
            assert_eq!(
                fs::read_to_string(dir.path().join("d000/f000.txt")).expect("visible file"),
                if name == "sparse-base" {
                    "same\n"
                } else {
                    "edited\n"
                }
            );
            assert!(!dir.path().join("d001").exists());
        }
        git(&["fsck", "--strict"]);
        // Git can keep working on the index grit wrote.
        git(&["reset", "--hard", "sparse-edited"]);
        assert_eq!(git(&["ls-files", "--sparse"]).lines().count(), 700);
    }

    #[test]
    fn sparse_tree_walk_keeps_matching_excluded_subtree_opaque() {
        let (dir, repo, t1, _) = two_commit_repo();
        let unchanged = subtree_oid(&repo, &t1, b"a");
        let mut placeholder = entry("a/", unchanged, MODE_TREE, 0);
        placeholder.set_skip_worktree(true);
        let seed = index_of(vec![placeholder]);
        // An unchanged sparse tree must not need any descendant objects or rows.
        fs::remove_file(loose_object_path(&repo, &unchanged)).expect("drop excluded tree");
        drop(repo);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("reopen");
        let target = index_from_tree_reusing_cache_tree(&repo, &t1, &seed)
            .expect("unchanged excluded tree stays opaque");
        assert!(target
            .get(b"a/", 0)
            .expect("placeholder")
            .is_sparse_directory_placeholder());
        assert!(!target
            .entries
            .iter()
            .any(|e| e.path.starts_with(b"a/") && e.path != b"a/"));
        assert_eq!(target.cache_tree_root, Some(t1));
        assert_eq!(
            target.cache_tree.as_ref().expect("cache tree").entry_count as usize,
            target.entries.len()
        );
    }

    /// [`two_commit_repo`] turned into a cone-mode sparse-index checkout of the top level only:
    /// `a/` and `c/` become sparse-directory rows and leave the work tree. Returns the reopened
    /// repository and the commits of `t1` and `t2`.
    fn sparse_two_commit_repo() -> (TempDir, Repository, ObjectId, ObjectId, ObjectId, ObjectId) {
        let (dir, repo, t1, t2) = two_commit_repo();
        let head = *resolve_head_for_repository(&repo)
            .expect("head")
            .oid()
            .expect("oid");
        let commit = repo.odb().read(&head).expect("commit");
        let parent = crate::objects::parse_commit(&commit.data)
            .expect("parse")
            .parents[0];
        let git_dir = repo.git_dir().to_path_buf();
        drop(repo);
        let mut config = fs::read_to_string(git_dir.join("config")).expect("config");
        config.push_str("[core]\n\tsparseCheckout = true\n\tsparseCheckoutCone = true\n");
        config.push_str("[index]\n\tsparse = true\n");
        fs::write(git_dir.join("config"), config).expect("write config");
        fs::create_dir_all(git_dir.join("info")).expect("info");
        fs::write(git_dir.join("info/sparse-checkout"), "/*\n!/*/\n").expect("patterns");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("reopen");
        let mut index = repo.load_index().expect("index");
        for entry in &mut index.entries {
            if entry.path.contains(&b'/') {
                entry.set_skip_worktree(true);
            }
        }
        fs::remove_dir_all(dir.path().join("a")).expect("rm a");
        fs::remove_dir_all(dir.path().join("c")).expect("rm c");
        repo.write_index(&mut index).expect("write sparse index");
        let raw = Index::load(&repo.index_path()).expect("raw index");
        let dirs: Vec<&[u8]> = raw
            .entries
            .iter()
            .filter(|e| e.is_sparse_directory_placeholder())
            .map(|e| e.path.as_slice())
            .collect();
        assert_eq!(dirs, [b"a/".as_slice(), b"c/".as_slice()]);
        (dir, repo, t1, t2, parent, head)
    }

    #[test]
    fn hard_reset_load_retargets_changed_absent_sparse_directories() {
        let (_dir, repo, t1, t2, _, _) = sparse_two_commit_repo();
        let old_c = subtree_oid(&repo, &t2, b"c");
        let new_c = subtree_oid(&repo, &t1, b"c");
        assert_ne!(old_c, new_c);

        let index = load_index_for_hard_reset(&repo, &t1).expect("hard reset seed");
        let c = index.get(b"c/", 0).expect("c/ stays opaque");
        assert!(c.is_sparse_directory_placeholder() && c.skip_worktree());
        assert_eq!(c.oid, new_c);
        assert_eq!(
            index.get(b"a/", 0).expect("a/").oid,
            subtree_oid(&repo, &t1, b"a")
        );
        assert!(!index.entries.iter().any(|e| e.path.starts_with(b"c/z")));
        assert_eq!(
            index.cache_tree_root, None,
            "retargeting invalidates the root"
        );

        // Other transitions keep checking staged content, so a changed tree is still expanded.
        let mut transition = repo.load_index_preserving_sparse().expect("index");
        prepare_index_for_tree_transition(&repo, &t1, &mut transition).expect("transition");
        assert!(transition.get(b"c/", 0).is_none());
        assert_eq!(
            transition.get(b"c/z", 0).expect("c/z").oid,
            repo.odb().write(ObjectKind::Blob, b"z2\n").expect("blob")
        );
        assert!(
            transition.get(b"a/", 0).is_some(),
            "unchanged a/ stays opaque"
        );
    }

    #[test]
    fn sparse_hard_reset_never_reads_or_expands_changed_excluded_trees() {
        use crate::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
        use crate::progress::NeverCancel;
        let (dir, repo, t1, t2, c1, c2) = sparse_two_commit_repo();
        let tree_c1 = subtree_oid(&repo, &t1, b"c");
        let tree_c2 = subtree_oid(&repo, &t2, b"c");
        // Neither side's excluded subtree may be needed by the reset.
        for oid in [tree_c1, tree_c2] {
            fs::remove_file(loose_object_path(&repo, &oid)).expect("drop c tree");
        }
        drop(repo);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("reopen");
        for (commit, tree, c_tree) in [(c1, t1, tree_c1), (c2, t2, tree_c2), (c1, t1, tree_c1)] {
            reset_to_commit(
                &repo,
                &ResetOptions::new(
                    commit,
                    ResetMode::Hard,
                    "T <t@example.com> 2 +0000".parse().expect("signature"),
                    "reset",
                ),
                &mut NullProgress,
                &NeverCancel,
            )
            .expect("reset");
            assert_eq!(repo.odb().hot_path_metrics().sparse_expanded_rows(), 0);
            let raw = Index::load(&repo.index_path()).expect("raw index");
            let rows: Vec<(&[u8], ObjectId)> = raw
                .entries
                .iter()
                .map(|e| (e.path.as_slice(), e.oid))
                .collect();
            assert_eq!(
                rows,
                [
                    (b"a/".as_slice(), subtree_oid(&repo, &tree, b"a")),
                    (b"c/".as_slice(), c_tree),
                    (b"top".as_slice(), raw.entries[2].oid),
                ]
            );
            assert!(raw.entries[..2].iter().all(|e| e.skip_worktree()));
            assert_eq!(raw.cache_tree_root, Some(tree));
            crate::write_tree::verify_cache_tree(&raw).expect("sparse cache-tree");
            assert!(!dir.path().join("c").exists());
            assert_eq!(fs::read(dir.path().join("top")).expect("top"), b"t\n");
        }
    }

    #[test]
    fn sparse_directory_diff_skips_opaque_subtrees_only() {
        let (dir, repo, t1, t2, _, _) = sparse_two_commit_repo();
        let index = repo.load_index_preserving_sparse().expect("sparse index");
        let pruned = crate::diff::diff_trees_outside_sparse_directories(
            repo.odb(),
            &index,
            Some(&t2),
            Some(&t1),
        )
        .expect("pruned diff");
        assert!(pruned.is_empty(), "only c/z changed, inside c/: {pruned:?}");
        let full = crate::diff::diff_trees(repo.odb(), Some(&t2), Some(&t1), "").expect("diff");
        assert_eq!(full.len(), 1);
        assert_eq!(full[0].new_path.as_deref(), Some("c/z"));

        // A plain index prunes nothing, and the subtree objects are really skipped when opaque.
        let plain = Index::new();
        let unpruned = crate::diff::diff_trees_outside_sparse_directories(
            repo.odb(),
            &plain,
            Some(&t2),
            Some(&t1),
        )
        .expect("unpruned");
        assert_eq!(unpruned, full);
        for tree in [t1, t2] {
            fs::remove_file(loose_object_path(&repo, &subtree_oid(&repo, &tree, b"c")))
                .expect("drop c tree");
        }
        drop(repo);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("reopen");
        assert!(crate::diff::diff_trees_outside_sparse_directories(
            repo.odb(),
            &index,
            Some(&t2),
            Some(&t1),
        )
        .expect("no subtree reads")
        .is_empty());
    }

    #[test]
    fn reusing_walk_does_not_read_subtrees_the_seed_cache_tree_covers() {
        let (dir, repo, t1, _t2) = two_commit_repo();
        let seed = repo.load_index().expect("index");
        let unchanged = subtree_oid(&repo, &t1, b"a");
        fs::remove_file(loose_object_path(&repo, &unchanged)).expect("drop subtree object");
        drop(repo);
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("reopen");

        let reused = index_from_tree_reusing_cache_tree(&repo, &t1, &seed).expect("walk");
        assert!(reused.get(b"a/b/y", 0).is_some());
        assert_eq!(reused.cache_tree_root, Some(t1));
        assert!(
            index_from_tree(&repo, &t1).is_err(),
            "a full tree read needs the dropped object"
        );
    }

    #[test]
    fn reusing_walk_ignores_cache_tree_nodes_that_disagree_with_the_rows() {
        let (_dir, repo, t1, t2) = two_commit_repo();
        let at_t2 = repo.load_index().expect("index");
        let any_oid = at_t2.entries[0].oid;

        let mut missing_row = at_t2.clone();
        missing_row.entries.retain(|e| e.path != b"a/x");
        let mut extra_row = at_t2.clone();
        extra_row.add_or_replace(entry("a/new", any_oid, MODE_REGULAR, 0));
        let mut unmerged = at_t2.clone();
        let mut stage2 = unmerged.get(b"a/x", 0).expect("a/x").clone();
        stage2.set_stage(2);
        unmerged.add_or_replace(stage2);
        unmerged.sort();

        for seed in [&missing_row, &extra_row, &unmerged] {
            for target in [t1, t2] {
                let reused =
                    index_from_tree_reusing_cache_tree(&repo, &target, seed).expect("walk");
                assert_same_index(&reused, &plain_tree_index(&repo, &target));
            }
        }
    }

    #[test]
    fn owned_seed_at_target_tree_matches_borrowed_seed() {
        let (_dir, repo, t1, t2) = two_commit_repo();
        let mut seed = repo.load_index().expect("index");
        seed.get_mut(b"a/x", 0)
            .expect("a/x")
            .set_assume_unchanged(true);
        seed.get_mut(b"top", 0)
            .expect("top")
            .set_skip_worktree(true);
        let never_stat = seed.get_mut(b"c/z", 0).expect("c/z");
        never_stat.size = 0;
        never_stat.mtime_sec = 0;
        never_stat.mtime_nsec = 0;

        let borrowed = index_from_tree_with_seeded_stats(&repo, &t2, &seed).expect("borrowed");
        for keep_skip_worktree in [false, true] {
            let owned = index_from_matching_seed(&mut seed.clone(), &t2, keep_skip_worktree)
                .expect("seed matches t2");
            if keep_skip_worktree {
                assert!(owned.get(b"top", 0).expect("top").skip_worktree());
            } else {
                assert_same_index(&owned, &borrowed);
            }
        }
        let owned = index_from_matching_seed(&mut seed.clone(), &t2, false).expect("matches");
        assert!(owned.get(b"a/x", 0).expect("a/x").stat_cache_unset());
        assert!(!owned.get(b"a/b/y", 0).expect("a/b/y").stat_cache_unset());

        let mut unmatched = seed.clone();
        assert!(index_from_matching_seed(&mut unmatched, &t1, false).is_none());
        assert_eq!(unmatched.entries, seed.entries);
        assert_eq!(unmatched.cache_tree, seed.cache_tree);
    }
}
