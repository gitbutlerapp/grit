//! Cached upstream delta for replaying commits across a moved first parent (rebase onto).

#[cfg(any(test, feature = "unstable-bench-helpers"))]
use std::cell::Cell;
use std::collections::{HashMap, HashSet};

use crate::diff::{detect_renames, diff_trees, DiffEntry, DiffStatus};
use crate::index::{Index, IndexEntry, MODE_TREE};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    lookup_index_entry_at_path_cached, merge_trees_replay_transplant, merge_trees_three_way_cached,
    DirectoryRenameMode, MergeTreeCache, ReplayConflictLabels, TreeMergeOutput,
    WhitespaceMergeOptions,
};
use crate::objects::FileMode;
use crate::objects::ObjectId;
use crate::porcelain::cherry_pick::directory_renames_from_file_renames;
use crate::repo::Repository;
use crate::write_tree::write_tree_partial_from_index;

use super::error::HistoryError;

thread_local! { // hygiene: replay merge metrics for tests and unstable-bench-helpers
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    static REPLAY_MERGE_METRICS: Cell<(usize, usize)> = const { Cell::new((0, 0)) };
}

/// Test hook: number of three-way tree merges performed by planning.
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[doc(hidden)]
#[must_use]
pub fn test_merge_tree_call_count() -> usize {
    REPLAY_MERGE_METRICS.with(|c| c.get().0)
}

/// Reset [`test_merge_tree_call_count`].
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[doc(hidden)]
pub fn test_reset_merge_tree_call_count() {
    REPLAY_MERGE_METRICS.with(|c| c.set((0, 0)));
}

/// Test hook: full fork→onto diff/rename scans during replay planning.
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[doc(hidden)]
#[must_use]
pub fn test_upstream_delta_scan_count() -> usize {
    REPLAY_MERGE_METRICS.with(|c| c.get().1)
}

/// Reset [`test_upstream_delta_scan_count`].
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[doc(hidden)]
pub fn test_reset_upstream_delta_scan_count() {
    REPLAY_MERGE_METRICS.with(|c| c.set((0, 0)));
}

pub(crate) fn record_merge_call() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    REPLAY_MERGE_METRICS.with(|c| {
        let (merges, scans) = c.get();
        c.set((merges + 1, scans));
    });
}

fn record_upstream_delta_scan() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    REPLAY_MERGE_METRICS.with(|c| {
        let (merges, scans) = c.get();
        c.set((merges, scans + 1));
    });
}

struct UpstreamDeltaCache {
    fork_tree: ObjectId,
    onto_tree: ObjectId,
    touched: HashSet<Vec<u8>>,
    file_old_to_new: HashMap<Vec<u8>, Vec<u8>>,
    fork_onto_rename_triples: Vec<RenameTriple>,
    dir_rename_roots: HashSet<Vec<u8>>,
}

enum OverlapTransplant {
    Applied(ObjectId),
    Conflict(TreeMergeOutput),
    NotApplicable,
}

/// Reuse upstream fork↔onto analysis across many replay transplants in one plan.
#[derive(Default)]
pub(crate) struct ReplayMergeSession {
    merge_cache: MergeTreeCache,
    upstream: Option<UpstreamDeltaCache>,
    directory_renames: Option<DirectoryRenameMode>,
    replayed_touched: HashSet<Vec<u8>>,
}

impl ReplayMergeSession {
    fn directory_renames(
        &mut self,
        repo: &Repository,
    ) -> Result<DirectoryRenameMode, HistoryError> {
        match self.directory_renames {
            Some(mode) => Ok(mode),
            None => {
                let mode = DirectoryRenameMode::from_repo(repo).map_err(HistoryError::from)?;
                self.directory_renames = Some(mode);
                Ok(mode)
            }
        }
    }

    fn ensure_upstream(
        &mut self,
        repo: &Repository,
        fork_tree: ObjectId,
        onto_tree: ObjectId,
    ) -> Result<(), HistoryError> {
        if self.upstream.is_some() {
            return Ok(());
        }
        self.build_upstream_cache(repo, fork_tree, onto_tree)
    }

    fn build_upstream_cache(
        &mut self,
        repo: &Repository,
        fork_tree: ObjectId,
        onto_tree: ObjectId,
    ) -> Result<(), HistoryError> {
        record_upstream_delta_scan();
        let odb = repo.odb();
        let entries =
            diff_trees(odb, Some(&fork_tree), Some(&onto_tree), "").map_err(HistoryError::from)?;
        let touched = paths_from_diff_entries(&entries);
        let rename_pairs = rename_pairs_from_diff(odb, entries)?;
        let fork_onto_rename_triples = rename_pairs.clone();
        let mut file_old_to_new = HashMap::new();
        for (old, new, _) in &rename_pairs {
            if old != new {
                file_old_to_new.insert(old.clone(), new.clone());
            }
        }
        let dir_renames = directory_renames_from_file_renames(
            &rename_pairs
                .iter()
                .map(|(o, n, _)| (o.clone(), n.clone()))
                .collect::<Vec<_>>(),
        );
        let mut dir_rename_roots = HashSet::new();
        for (old, new, _) in &rename_pairs {
            if is_tree_path(&mut self.merge_cache, odb, &fork_tree, old)?
                && is_tree_path(&mut self.merge_cache, odb, &fork_tree, new)?
            {
                dir_rename_roots.insert(old.clone());
                dir_rename_roots.insert(new.clone());
            }
        }
        for dir in dir_renames.keys().chain(dir_renames.values()) {
            dir_rename_roots.insert(dir.clone());
        }
        self.upstream = Some(UpstreamDeltaCache {
            fork_tree,
            onto_tree,
            touched,
            file_old_to_new,
            fork_onto_rename_triples,
            dir_rename_roots,
        });
        Ok(())
    }

    fn upstream_cache_still_valid(
        &mut self,
        repo: &Repository,
        old_parent_tree: ObjectId,
        new_parent_tree: ObjectId,
    ) -> Result<bool, HistoryError> {
        let Some(upstream) = &self.upstream else {
            return Ok(true);
        };
        let fork_tree = upstream.fork_tree;
        let onto_tree = upstream.onto_tree;
        let replayed = self.replayed_touched.clone();
        if !diff_only_touches_replayed(
            repo,
            &mut self.merge_cache,
            fork_tree,
            old_parent_tree,
            &replayed,
        )? {
            return Ok(false);
        }
        diff_only_touches_replayed(
            repo,
            &mut self.merge_cache,
            onto_tree,
            new_parent_tree,
            &replayed,
        )
    }

    #[expect(clippy::too_many_arguments)]
    pub(crate) fn transplant_tree(
        &mut self,
        repo: &Repository,
        commit: ObjectId,
        labels: &ReplayConflictLabels,
        subject_tree: ObjectId,
        old_parent_tree: ObjectId,
        new_parent_tree: ObjectId,
        merge_favor: MergeFavor,
        on_conflict: impl FnOnce(ObjectId, TreeMergeOutput) -> Result<ObjectId, HistoryError>,
    ) -> Result<ObjectId, HistoryError> {
        if old_parent_tree == new_parent_tree {
            return Ok(subject_tree);
        }

        if merge_favor == MergeFavor::default() && self.upstream.is_none() {
            self.ensure_upstream(repo, old_parent_tree, new_parent_tree)?;
        }

        if merge_favor == MergeFavor::default() {
            if let Some(result) =
                self.try_fast_transplant(repo, old_parent_tree, new_parent_tree, subject_tree)?
            {
                self.note_replayed_commit(repo, old_parent_tree, subject_tree)?;
                return Ok(result);
            }
        }

        if merge_favor == MergeFavor::default() {
            if self.upstream_cache_still_valid(repo, old_parent_tree, new_parent_tree)? {
                match self.try_overlap_transplant(
                    repo,
                    labels,
                    subject_tree,
                    old_parent_tree,
                    new_parent_tree,
                )? {
                    OverlapTransplant::Applied(result) => {
                        self.note_replayed_commit(repo, old_parent_tree, subject_tree)?;
                        return Ok(result);
                    }
                    OverlapTransplant::Conflict(merge) => return on_conflict(commit, merge),
                    OverlapTransplant::NotApplicable => {}
                }
            } else {
                self.upstream = None;
            }
        }

        record_merge_call();
        let directory_renames = self.directory_renames(repo)?;
        let merge = merge_trees_three_way_cached(
            repo,
            &mut self.merge_cache,
            old_parent_tree,
            new_parent_tree,
            subject_tree,
            merge_favor,
            WhitespaceMergeOptions::default(),
            None,
            labels.presentation(crate::merge_file::ConflictStyle::Merge),
            directory_renames,
        )
        .map_err(HistoryError::from)?;

        if crate::operation::tree_merge_has_conflicts(&merge) {
            return on_conflict(commit, merge);
        }
        self.note_replayed_commit(repo, old_parent_tree, subject_tree)?;
        Ok(merge.result_tree)
    }

    fn note_replayed_commit(
        &mut self,
        repo: &Repository,
        old_parent_tree: ObjectId,
        subject_tree: ObjectId,
    ) -> Result<(), HistoryError> {
        let odb = repo.odb();
        let entries = diff_trees(odb, Some(&old_parent_tree), Some(&subject_tree), "")
            .map_err(HistoryError::from)?;
        self.replayed_touched
            .extend(paths_from_diff_entries(&entries));
        Ok(())
    }

    fn try_fast_transplant(
        &mut self,
        repo: &Repository,
        old_parent_tree: ObjectId,
        new_parent_tree: ObjectId,
        subject_tree: ObjectId,
    ) -> Result<Option<ObjectId>, HistoryError> {
        let Some(upstream) = self.upstream.as_ref() else {
            return Ok(None);
        };
        let odb = repo.odb();
        let commit_diff = diff_trees(odb, Some(&old_parent_tree), Some(&subject_tree), "")
            .map_err(HistoryError::from)?;
        if commit_diff.is_empty() {
            return Ok(Some(new_parent_tree));
        }
        if !commit_diff.iter().all(commit_change_is_simple) {
            return Ok(None);
        }
        let mut paths_from_index = HashSet::new();
        for entry in &commit_diff {
            let paths = diff_entry_paths(entry);
            for path in &paths {
                if touches_upstream(path, &upstream.touched)
                    || touches_dir_rename_root(path, &upstream.dir_rename_roots)
                {
                    return Ok(None);
                }
            }
            paths_from_index.extend(paths);
        }
        for path in &paths_from_index {
            if !parents_agree_at_path(
                &mut self.merge_cache,
                odb,
                old_parent_tree,
                new_parent_tree,
                path,
            )? {
                return Ok(None);
            }
            if let Some(parent) = parent_dir(path) {
                if !parent_dir_ready(
                    &mut self.merge_cache,
                    odb,
                    old_parent_tree,
                    new_parent_tree,
                    &parent,
                )? {
                    return Ok(None);
                }
            }
        }
        let index = index_from_commit_diff(repo, &commit_diff)?;
        let result =
            write_tree_partial_from_index(odb, &index, &new_parent_tree, &paths_from_index)
                .map_err(HistoryError::from)?;
        Ok(Some(result))
    }

    fn try_overlap_transplant(
        &mut self,
        repo: &Repository,
        labels: &ReplayConflictLabels,
        subject_tree: ObjectId,
        old_parent_tree: ObjectId,
        new_parent_tree: ObjectId,
    ) -> Result<OverlapTransplant, HistoryError> {
        let Some(upstream) = &self.upstream else {
            return Ok(OverlapTransplant::NotApplicable);
        };
        let file_old_to_new = upstream.file_old_to_new.clone();
        let fork_onto_rename_triples = upstream.fork_onto_rename_triples.clone();
        let upstream_touched = upstream.touched.clone();
        let dir_rename_roots = upstream.dir_rename_roots.clone();
        let odb = repo.odb();
        let commit_diff = diff_trees(odb, Some(&old_parent_tree), Some(&subject_tree), "")
            .map_err(HistoryError::from)?;
        if commit_diff.is_empty() {
            return Ok(OverlapTransplant::Applied(new_parent_tree));
        }
        if !commit_diff.iter().all(commit_change_is_simple) {
            return Ok(OverlapTransplant::NotApplicable);
        }
        let has_overlap = commit_diff.iter().any(|entry| {
            diff_entry_paths(entry).into_iter().any(|path| {
                touches_upstream(&path, &upstream_touched)
                    || touches_dir_rename_root(&path, &dir_rename_roots)
            })
        });
        if !has_overlap {
            return Ok(OverlapTransplant::NotApplicable);
        }

        let directory_renames = self.directory_renames(repo)?;
        let merge = merge_trees_replay_transplant(
            repo,
            &mut self.merge_cache,
            old_parent_tree,
            new_parent_tree,
            subject_tree,
            commit_diff,
            &file_old_to_new,
            &fork_onto_rename_triples,
            &upstream_touched,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            labels.presentation(crate::merge_file::ConflictStyle::Merge),
            directory_renames,
        )
        .map_err(HistoryError::from)?;

        if crate::operation::tree_merge_has_conflicts(&merge) {
            return Ok(OverlapTransplant::Conflict(merge));
        }
        Ok(OverlapTransplant::Applied(merge.result_tree))
    }
}

fn diff_only_touches_replayed(
    repo: &Repository,
    cache: &mut MergeTreeCache,
    from_tree: ObjectId,
    to_tree: ObjectId,
    replayed: &HashSet<Vec<u8>>,
) -> Result<bool, HistoryError> {
    if from_tree == to_tree {
        return Ok(true);
    }
    let odb = repo.odb();
    let entries =
        diff_trees(odb, Some(&from_tree), Some(&to_tree), "").map_err(HistoryError::from)?;
    for entry in entries {
        for path in diff_entry_paths(&entry) {
            if replayed.contains(&path) {
                continue;
            }
            if replayed.iter().any(|p| paths_overlap(&path, p)) {
                continue;
            }
            if parent_dir(&path).is_some_and(|d| replayed.contains(&d)) {
                continue;
            }
            return Ok(false);
        }
    }
    let _ = cache;
    Ok(true)
}

fn paths_overlap(a: &[u8], b: &[u8]) -> bool {
    a.starts_with(b) && (a.len() == b.len() || a.get(b.len()) == Some(&b'/'))
        || b.starts_with(a) && (b.len() == a.len() || b.get(a.len()) == Some(&b'/'))
}

fn paths_from_diff_entries(entries: &[DiffEntry]) -> HashSet<Vec<u8>> {
    let mut out = HashSet::new();
    for e in entries {
        out.extend(diff_entry_paths(e));
    }
    out
}

fn diff_entry_paths(entry: &DiffEntry) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    if let Some(p) = &entry.old_path {
        out.push(crate::repo_relpath::storage_bytes(p).into_owned());
    }
    if let Some(p) = &entry.new_path {
        let b = crate::repo_relpath::storage_bytes(p).into_owned();
        if !out.contains(&b) {
            out.push(b);
        }
    }
    out
}

type RenameTriple = (Vec<u8>, Vec<u8>, u32);

fn rename_pairs_from_diff(
    odb: &crate::odb::Odb,
    entries: Vec<DiffEntry>,
) -> Result<Vec<RenameTriple>, HistoryError> {
    let entries = detect_renames(odb, None, entries, 50);
    let mut out = Vec::new();
    for e in entries {
        if e.status != DiffStatus::Renamed {
            continue;
        }
        let Some(old) = e.old_path else {
            continue;
        };
        let Some(new) = e.new_path else {
            continue;
        };
        let score = e.score.unwrap_or(0);
        out.push((
            crate::repo_relpath::storage_bytes(&old).into_owned(),
            crate::repo_relpath::storage_bytes(&new).into_owned(),
            score,
        ));
    }
    Ok(out)
}

fn is_tree_path(
    cache: &mut MergeTreeCache,
    odb: &crate::odb::Odb,
    tree: &ObjectId,
    path: &[u8],
) -> Result<bool, HistoryError> {
    Ok(lookup_index_entry_at_path_cached(cache, odb, tree, path)
        .map_err(HistoryError::from)?
        .is_some_and(|e| e.mode == MODE_TREE))
}

fn touches_upstream(path: &[u8], upstream: &HashSet<Vec<u8>>) -> bool {
    if upstream.contains(path) {
        return true;
    }
    for p in upstream {
        if path.starts_with(p) && (path.len() == p.len() || path.get(p.len()) == Some(&b'/')) {
            return true;
        }
        if p.starts_with(path) && (p.len() == path.len() || p.get(p.len()) == Some(&b'/')) {
            return true;
        }
    }
    false
}

fn touches_dir_rename_root(path: &[u8], roots: &HashSet<Vec<u8>>) -> bool {
    for root in roots {
        if path.starts_with(root)
            && (path.len() == root.len() || path.get(root.len()) == Some(&b'/'))
        {
            return true;
        }
    }
    false
}

fn parent_dir(path: &[u8]) -> Option<Vec<u8>> {
    path.iter()
        .rposition(|&b| b == b'/')
        .map(|pos| path[..pos].to_vec())
}

fn parents_agree_at_path(
    cache: &mut MergeTreeCache,
    odb: &crate::odb::Odb,
    old_parent: ObjectId,
    new_parent: ObjectId,
    path: &[u8],
) -> Result<bool, HistoryError> {
    let old_e = lookup_index_entry_at_path_cached(cache, odb, &old_parent, path)
        .map_err(HistoryError::from)?;
    let new_e = lookup_index_entry_at_path_cached(cache, odb, &new_parent, path)
        .map_err(HistoryError::from)?;
    Ok(match (old_e, new_e) {
        (None, None) => true,
        (Some(a), Some(b)) => a.mode == b.mode && a.oid == b.oid,
        _ => false,
    })
}

fn parent_dir_ready(
    cache: &mut MergeTreeCache,
    odb: &crate::odb::Odb,
    old_parent: ObjectId,
    new_parent: ObjectId,
    dir: &[u8],
) -> Result<bool, HistoryError> {
    let old_e = lookup_index_entry_at_path_cached(cache, odb, &old_parent, dir)
        .map_err(HistoryError::from)?;
    let new_e = lookup_index_entry_at_path_cached(cache, odb, &new_parent, dir)
        .map_err(HistoryError::from)?;
    Ok(match (old_e, new_e) {
        (None, None) => true,
        (Some(a), Some(b)) if a.mode == MODE_TREE && b.mode == MODE_TREE => a.oid == b.oid,
        _ => false,
    })
}

fn commit_change_is_simple(entry: &DiffEntry) -> bool {
    matches!(
        entry.status,
        DiffStatus::Added | DiffStatus::Modified | DiffStatus::Deleted | DiffStatus::Renamed
    ) && entry.old_mode != FileMode::GITLINK
        && entry.new_mode != FileMode::GITLINK
        && entry.old_mode != FileMode::SYMLINK
        && entry.new_mode != FileMode::SYMLINK
}

fn index_from_commit_diff(repo: &Repository, diff: &[DiffEntry]) -> Result<Index, HistoryError> {
    let mut index = Index::new();
    index.hash_algo = repo.odb().hash_algo();
    for entry in diff {
        match entry.status {
            DiffStatus::Added | DiffStatus::Modified | DiffStatus::Renamed => {
                let Some(path) = entry.new_path.as_ref() else {
                    continue;
                };
                let path_bytes = crate::repo_relpath::storage_bytes(path).into_owned();
                index.add_or_replace(index_entry_from_diff(entry, path_bytes));
            }
            DiffStatus::Deleted => {}
            _ => return Ok(Index::new()),
        }
    }
    Ok(index)
}

fn index_entry_from_diff(entry: &DiffEntry, path: Vec<u8>) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: entry.new_mode.bits(),
        uid: 0,
        gid: 0,
        size: 0,
        oid: entry.new_oid,
        flags: path.len().min(0xfff) as u16,
        flags_extended: None,
        path,
        base_index_pos: 0,
    }
}
