//! Lightweight worktree cleanliness checks without a full [`status`](super::status::status) pass.
//!
//! Pick, merge, and branch switch only need to know whether staged or unstaged changes exist
//! (and switch additionally needs untracked paths plus a tree-to-tree diff). This module loads
//! the index once, refreshes stat data when needed, and computes those slices without rename
//! detection, ahead/behind, stash counts, or in-progress operation state.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::config::ConfigSet;
use crate::diff::format_mode;
use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, DiffEntry, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, Index};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use crate::repo::Repository;
use crate::state::resolve_head;
use crate::worktree_rules::WorktreeRules;

/// Loaded index and HEAD tree for worktree mutation guards.
pub struct WorktreeSnapshot {
    /// Index with sparse-directory placeholders expanded.
    pub index: Index,
    /// Tree OID of the current HEAD commit, if any.
    pub head_tree: Option<ObjectId>,
}

/// Load the index and resolve HEAD's tree OID (no worktree scan).
///
/// # Errors
///
/// Returns errors from index I/O, ODB reads, or when the repository has no work tree.
pub fn load_worktree_snapshot(repo: &Repository) -> Result<WorktreeSnapshot> {
    let _work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    let index_path = repo.index_path_for_env()?;
    let mut index = repo.load_index_at(&index_path)?;
    let _ = index.expand_sparse_directory_placeholders(repo.odb());

    let head = resolve_head(repo.git_dir())?;
    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb().read(oid)?;
            Some(parse_commit(&obj.data)?.tree)
        }
        None => None,
    };

    Ok(WorktreeSnapshot { index, head_tree })
}

/// Staged and unstaged diffs for the current worktree (no rename detection), with stat refresh
/// in the same worktree pass as the unstaged diff.
pub(crate) fn local_change_diffs(
    repo: &Repository,
    index: &mut Index,
    head_tree: Option<ObjectId>,
) -> Result<(Vec<DiffEntry>, Vec<DiffEntry>, bool)> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path_for_env()?;
    let index_mtime = index_file_mtime(&index_path);
    let config = ConfigSet::load(repo.environment(), Some(repo.git_dir()), true)
        .ok()
        .map(Arc::new);
    let worktree_rules = match config.as_ref() {
        Some(cfg) => Some(Arc::new(Mutex::new(WorktreeRules::from_parts(
            repo,
            index,
            cfg.clone(),
        )?))),
        None => None,
    };

    let staged = diff_index_to_tree(repo.odb(), index, head_tree.as_ref(), false)?;
    let (unstaged, index_changed) = diff_index_to_worktree_with_options(
        repo.odb(),
        index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            index_mtime: crate::diff::index_mtime_for_diff(index, index_mtime),
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            refresh_index_stat_in_pass: false,
            config,
            worktree_rules,
            ..Default::default()
        },
    )?;
    Ok((staged, unstaged, index_changed))
}

/// Like [`local_change_diffs`], but only considers paths touched by `tree_changes`.
///
/// Falls back to a full index/worktree scan when the index has unmerged entries.
pub(crate) fn local_change_diffs_for_tree_changes(
    repo: &Repository,
    index: &mut Index,
    head_tree: Option<ObjectId>,
    tree_changes: &[DiffEntry],
) -> Result<(Vec<DiffEntry>, Vec<DiffEntry>, bool)> {
    let _ = head_tree;
    if index.entries.iter().any(|e| e.stage() != 0) {
        return local_change_diffs(repo, index, head_tree);
    }
    let paths = paths_touched_by_tree_changes(tree_changes);
    if paths.is_empty() {
        return Ok((Vec::new(), Vec::new(), false));
    }
    let path_filter = Arc::new(paths);
    let head_by_path = head_tree_sides_from_changes(tree_changes);
    let staged = staged_diff_for_tree_switch_paths(index, &head_by_path, path_filter.as_ref())?;

    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path_for_env()?;
    let index_mtime = index_file_mtime(&index_path);
    let config = ConfigSet::load(repo.environment(), Some(repo.git_dir()), true)
        .ok()
        .map(Arc::new);
    let (unstaged, index_changed) = diff_index_to_worktree_with_options(
        repo.odb(),
        index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            index_mtime: crate::diff::index_mtime_for_diff(index, index_mtime),
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            refresh_index_stat_in_pass: false,
            for_status: true,
            stat_parallel_threads: Some(1),
            config,
            restrict_to_paths: Some(path_filter),
            ..Default::default()
        },
    )?;
    Ok((staged, unstaged, index_changed))
}

#[derive(Clone, Copy)]
enum HeadTreeSide {
    Present { oid: ObjectId, mode: u32 },
    Absent,
}

fn parse_diff_mode(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or(0)
}

fn head_tree_sides_from_changes(tree_changes: &[DiffEntry]) -> HashMap<String, HeadTreeSide> {
    let mut map = HashMap::with_capacity(tree_changes.len());
    for change in tree_changes {
        let Some(path) = &change.old_path else {
            continue;
        };
        map.insert(
            path.clone(),
            HeadTreeSide::Present {
                oid: change.old_oid,
                mode: parse_diff_mode(&change.old_mode),
            },
        );
    }
    map
}

fn paths_touched_by_tree_changes(tree_changes: &[DiffEntry]) -> BTreeSet<String> {
    let mut paths = BTreeSet::new();
    for change in tree_changes {
        if let Some(path) = &change.old_path {
            paths.insert(path.clone());
        }
        if let Some(path) = &change.new_path {
            paths.insert(path.clone());
        }
    }
    paths
}

fn staged_diff_for_tree_switch_paths(
    index: &Index,
    head_by_path: &HashMap<String, HeadTreeSide>,
    paths: &BTreeSet<String>,
) -> Result<Vec<DiffEntry>> {
    let mut staged = Vec::new();
    for path in paths {
        let path_bytes = path.as_bytes();
        let index_entry = index.get(path_bytes, 0);
        if index_entry.is_some_and(|e| e.intent_to_add()) {
            continue;
        }
        let head = head_by_path
            .get(path)
            .copied()
            .unwrap_or(HeadTreeSide::Absent);
        match (head, index_entry) {
            (HeadTreeSide::Absent, None) => {}
            (HeadTreeSide::Absent, Some(ie)) => {
                staged.push(DiffEntry {
                    status: DiffStatus::Added,
                    old_path: None,
                    new_path: Some(path.clone()),
                    old_mode: String::new(),
                    new_mode: format_mode(ie.mode),
                    old_oid: ObjectId::zero(),
                    new_oid: ie.oid,
                    score: None,
                });
            }
            (HeadTreeSide::Present { oid, mode }, None) => {
                staged.push(DiffEntry {
                    status: DiffStatus::Deleted,
                    old_path: Some(path.clone()),
                    new_path: None,
                    old_mode: format_mode(mode),
                    new_mode: String::new(),
                    old_oid: oid,
                    new_oid: ObjectId::zero(),
                    score: None,
                });
            }
            (HeadTreeSide::Present { oid, mode }, Some(ie)) if ie.oid != oid || ie.mode != mode => {
                staged.push(DiffEntry {
                    status: if ie.mode != mode {
                        DiffStatus::TypeChanged
                    } else {
                        DiffStatus::Modified
                    },
                    old_path: Some(path.clone()),
                    new_path: Some(path.clone()),
                    old_mode: format_mode(mode),
                    new_mode: format_mode(ie.mode),
                    old_oid: oid,
                    new_oid: ie.oid,
                    score: None,
                });
            }
            (HeadTreeSide::Present { .. }, Some(_)) => {}
        }
    }
    Ok(staged)
}

/// Pathspec roots covering every [`DiffStatus::Added`] path and its parent directories.
///
/// Used before tree checkout to find untracked paths that would be overwritten without scanning
/// the whole work tree.
#[must_use]
pub(crate) fn untracked_scan_pathspecs_for_tree_changes(tree_changes: &[DiffEntry]) -> Vec<String> {
    let mut specs = BTreeSet::new();
    for change in tree_changes {
        if change.status != DiffStatus::Added {
            continue;
        }
        let Some(added) = &change.new_path else {
            continue;
        };
        let trimmed = added.trim_end_matches('/');
        if trimmed.is_empty() {
            continue;
        }
        specs.insert(trimmed.to_owned());
        let mut prefix = String::new();
        for part in trimmed.split('/') {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(part);
            specs.insert(prefix.clone());
        }
    }
    specs.into_iter().collect()
}

pub(crate) fn collect_untracked_for_tree_changes(
    repo: &Repository,
    index: &mut Index,
    work_tree: &Path,
    tree_changes: &[DiffEntry],
) -> Result<Vec<String>> {
    if !tree_changes.iter().any(|c| c.status == DiffStatus::Added) {
        return Ok(Vec::new());
    }
    let pathspecs = untracked_scan_pathspecs_for_tree_changes(tree_changes);
    let (untracked, _) =
        collect_untracked_and_ignored(repo, index, work_tree, IgnoredMode::No, true, &pathspecs)?;
    Ok(untracked)
}

fn ensure_worktree_clean_with_message(
    repo: &Repository,
    message: &str,
    persist_index_stat_refresh: bool,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let index_path = repo.index_path_for_env()?;
    let (staged, unstaged, index_changed) =
        local_change_diffs(repo, &mut snapshot.index, snapshot.head_tree)?;
    if persist_index_stat_refresh
        && index_changed
        && repo.try_write_index_at(&index_path, &mut snapshot.index)?
    {
        snapshot.index.source_mtime = index_file_mtime(&index_path);
    }
    if !staged.is_empty() || !unstaged.is_empty() {
        return Err(Error::Message(message.into()));
    }
    Ok(())
}

/// Refuse when the index or worktree differs from HEAD (staged or unstaged changes).
///
/// # Errors
///
/// Returns [`Error::Message`] when changes are present, matching `grit pick` wording.
pub fn ensure_worktree_clean_for_pick(repo: &Repository) -> Result<()> {
    ensure_worktree_clean_with_message(
        repo,
        "you have uncommitted changes — commit them before picking",
        false,
    )
}

/// Refuse when the index or worktree differs from HEAD (staged or unstaged changes).
///
/// # Errors
///
/// Returns [`Error::Message`] when changes are present, matching `grit merge` wording.
pub fn ensure_worktree_clean_for_merge(repo: &Repository) -> Result<()> {
    ensure_worktree_clean_with_message(
        repo,
        "you have uncommitted changes — commit them before merging",
        true,
    )
}

/// Inputs for a branch switch or fast-forward checkout after cleanliness checks.
pub struct TreeSwitchPlan {
    /// Untracked paths under the work tree (normal untracked mode).
    pub untracked: Vec<String>,
    /// Tree diff from `from_tree` to `to_tree`.
    pub tree_changes: Vec<DiffEntry>,
}

/// Verify the worktree is clean, collect untracked paths, and diff two trees in one index load.
///
/// # Errors
///
/// Returns [`Error::Message`] when staged or unstaged changes exist, or on I/O/ODB failures.
pub fn prepare_tree_switch(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<TreeSwitchPlan> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let index_path = repo.index_path_for_env()?;
    let (staged, unstaged, index_changed) =
        local_change_diffs(repo, &mut snapshot.index, snapshot.head_tree)?;
    if index_changed && repo.try_write_index_at(&index_path, &mut snapshot.index)? {
        snapshot.index.source_mtime = index_file_mtime(&index_path);
    }
    if !staged.is_empty() || !unstaged.is_empty() {
        return Err(Error::Message(
            "you have uncommitted changes — commit them before switching".into(),
        ));
    }

    let tree_changes = diff_trees(repo.odb(), from_tree, Some(to_tree), "")?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, &tree_changes)?;
    Ok(TreeSwitchPlan {
        untracked,
        tree_changes,
    })
}

/// `true` when `prefix` is a proper path prefix of `path` at a `/` component boundary.
fn is_component_prefix(prefix: &str, path: &str) -> bool {
    if path.len() <= prefix.len() {
        return false;
    }
    path.starts_with(prefix) && path.as_bytes().get(prefix.len()) == Some(&b'/')
}

/// Whether an added tree path would replace untracked content at `untracked` (exact or file/dir clash).
fn added_path_collides_with_untracked(added: &str, untracked: &str) -> bool {
    added == untracked
        || is_component_prefix(added, untracked)
        || is_component_prefix(untracked, added)
}

/// Paths of untracked worktree files that a tree checkout would replace.
///
/// Only tree diff entries with [`DiffStatus::Added`] are considered: the destination
/// tree introduces a new path that is not present in the source tree. Collisions include
/// exact matches and file/directory clashes at component boundaries (e.g. added `slot` vs
/// untracked `slot/precious.txt`, or the reverse).
#[must_use]
pub fn untracked_would_be_overwritten_by_tree_changes(
    untracked_paths: &[String],
    tree_changes: &[DiffEntry],
) -> Vec<String> {
    if untracked_paths.is_empty() {
        return Vec::new();
    }
    let mut collisions = Vec::new();
    for change in tree_changes {
        if change.status != DiffStatus::Added {
            continue;
        }
        let Some(added) = &change.new_path else {
            continue;
        };
        for untracked in untracked_paths {
            let u = untracked.trim_end_matches('/');
            if u.is_empty() {
                continue;
            }
            if added_path_collides_with_untracked(added, u) {
                collisions.push(u.to_owned());
            }
        }
    }
    collisions.sort();
    collisions.dedup();
    collisions
}

/// Refuse when [`untracked_would_be_overwritten_by_tree_changes`] would be non-empty.
///
/// # Errors
///
/// Returns [`Error::Message`] naming the first colliding path (same wording as `grit switch`).
pub fn ensure_no_untracked_overwrite(
    untracked_paths: &[String],
    tree_changes: &[DiffEntry],
) -> Result<()> {
    let collisions = untracked_would_be_overwritten_by_tree_changes(untracked_paths, tree_changes);
    if let Some(path) = collisions.first() {
        return Err(Error::Message(format!(
            "untracked file '{path}' would be overwritten — move or remove it first"
        )));
    }
    Ok(())
}

/// Collect untracked paths and refuse when updating the worktree from `from_tree` to `to_tree`
/// would clobber an untracked file.
///
/// Does not check for staged or unstaged changes; callers such as `grit merge` enforce a clean
/// worktree separately.
///
/// # Errors
///
/// Returns [`Error::Message`] when an untracked path would be overwritten, or on I/O/ODB failures.
pub fn prepare_tree_checkout(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<()> {
    let mut snapshot = load_worktree_snapshot(repo)?;
    let tree_changes = diff_trees(repo.odb(), from_tree, Some(to_tree), "")?;
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let untracked =
        collect_untracked_for_tree_changes(repo, &mut snapshot.index, work_tree, &tree_changes)?;
    ensure_no_untracked_overwrite(&untracked, &tree_changes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::diff_trees;
    use crate::repo::{init_repository, Repository};
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        std::fs::write(dir.path().join("tracked.txt"), "v1\n").expect("write");
        (dir, repo)
    }

    fn added_entry(path: &str) -> DiffEntry {
        DiffEntry {
            status: DiffStatus::Added,
            old_path: None,
            new_path: Some(path.to_owned()),
            old_mode: String::new(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score: None,
        }
    }

    fn modified_entry(path: &str) -> DiffEntry {
        DiffEntry {
            status: DiffStatus::Modified,
            old_path: Some(path.to_owned()),
            new_path: Some(path.to_owned()),
            old_mode: "100644".into(),
            new_mode: "100644".into(),
            old_oid: ObjectId::zero(),
            new_oid: ObjectId::zero(),
            score: None,
        }
    }

    #[test]
    fn untracked_collision_only_on_added_paths() {
        let untracked = vec!["new.txt".into(), "other.txt".into()];
        let changes = vec![
            added_entry("new.txt"),
            modified_entry("tracked.txt"),
            added_entry("missing.txt"),
        ];
        let hits = untracked_would_be_overwritten_by_tree_changes(&untracked, &changes);
        assert_eq!(hits, vec!["new.txt".to_owned()]);
    }

    #[test]
    fn untracked_collision_sorted_and_multiple() {
        let untracked = vec!["b.txt".into(), "a.txt".into()];
        let changes = vec![added_entry("b.txt"), added_entry("a.txt")];
        let hits = untracked_would_be_overwritten_by_tree_changes(&untracked, &changes);
        assert_eq!(hits, vec!["a.txt".to_owned(), "b.txt".to_owned()]);
    }

    #[test]
    fn untracked_collision_file_over_untracked_directory() {
        let untracked = vec!["slot/precious.txt".into()];
        let hits =
            untracked_would_be_overwritten_by_tree_changes(&untracked, &[added_entry("slot")]);
        assert_eq!(hits, vec!["slot/precious.txt".to_owned()]);
    }

    #[test]
    fn untracked_collision_directory_over_untracked_file() {
        let untracked = vec!["slot".into()];
        let hits = untracked_would_be_overwritten_by_tree_changes(
            &untracked,
            &[added_entry("slot/incoming.txt")],
        );
        assert_eq!(hits, vec!["slot".to_owned()]);
    }

    #[test]
    fn untracked_collision_allows_sibling_paths() {
        let untracked = vec!["slot/precious.txt".into()];
        let hits = untracked_would_be_overwritten_by_tree_changes(
            &untracked,
            &[added_entry("slot/incoming.txt")],
        );
        assert!(hits.is_empty());
    }

    #[test]
    fn untracked_scan_pathspecs_include_added_and_parents() {
        let changes = vec![
            added_entry("dir/sub/new.txt"),
            modified_entry("tracked.txt"),
            added_entry("top"),
        ];
        let specs = super::untracked_scan_pathspecs_for_tree_changes(&changes);
        assert!(specs.contains(&"dir/sub/new.txt".to_owned()));
        assert!(specs.contains(&"dir/sub".to_owned()));
        assert!(specs.contains(&"dir".to_owned()));
        assert!(specs.contains(&"top".to_owned()));
        assert!(!specs.contains(&"tracked.txt".to_owned()));
    }

    #[test]
    fn ensure_no_untracked_overwrite_err_message() {
        let err = ensure_no_untracked_overwrite(&["x.txt".into()], &[added_entry("x.txt")])
            .expect_err("collision");
        match err {
            Error::Message(msg) => {
                assert!(msg.contains("untracked file 'x.txt' would be overwritten"));
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn local_change_diffs_for_tree_changes_matches_full_on_modified_path() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        let mut index = repo.load_index().expect("index");
        let data = std::fs::read(wt.join("tracked.txt")).expect("read");
        let oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, &data)
            .expect("blob");
        let entry = crate::index::entry_from_stat(
            &wt.join("tracked.txt"),
            b"tracked.txt",
            oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        repo.write_index(&mut index).expect("write index");
        let head_tree = crate::write_tree::write_tree_update_index(
            &repo.odb(),
            &mut index,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .expect("tree");

        std::fs::write(wt.join("other.txt"), b"new\n").expect("write other");
        let other_data = b"new\n";
        let other_oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, other_data)
            .expect("blob");
        let mut index2 = repo.load_index().expect("index");
        let other_entry = crate::index::entry_from_stat(
            &wt.join("other.txt"),
            b"other.txt",
            other_oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index2.add_or_replace(other_entry);
        index2.sort();
        let target_tree = crate::write_tree::write_tree_update_index(
            &repo.odb(),
            &mut index2,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .expect("target tree");

        let tree_changes =
            diff_trees(repo.odb(), Some(&head_tree), Some(&target_tree), "").expect("diff");
        let mut index3 = repo.load_index().expect("index");
        let (full_staged, full_unstaged, _) =
            local_change_diffs(&repo, &mut index3, Some(head_tree)).expect("full");
        let mut index4 = repo.load_index().expect("index");
        let (scoped_staged, scoped_unstaged, _) =
            local_change_diffs_for_tree_changes(&repo, &mut index4, Some(head_tree), &tree_changes)
                .expect("scoped");
        assert_eq!(full_staged, scoped_staged);
        assert_eq!(full_unstaged, scoped_unstaged);
    }

    fn ensure_worktree_clean_for_pick_passes_on_fresh_commit() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree().expect("wt");
        let mut index = repo.load_index().expect("index");
        let data = std::fs::read(wt.join("tracked.txt")).expect("read");
        let oid = repo
            .odb()
            .write(crate::objects::ObjectKind::Blob, &data)
            .expect("blob");
        let entry = crate::index::entry_from_stat(
            &wt.join("tracked.txt"),
            b"tracked.txt",
            oid,
            crate::index::MODE_REGULAR,
        )
        .expect("entry");
        index.add_or_replace(entry);
        index.sort();
        repo.write_index(&mut index).expect("write index");
        let tree = crate::write_tree::write_tree_update_index(
            &repo.odb(),
            &mut index,
            "",
            crate::write_tree::WriteTreeFlags::silent(),
        )
        .expect("tree");
        let commit = crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "init\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        };
        let co = repo
            .odb()
            .write(
                crate::objects::ObjectKind::Commit,
                &crate::objects::serialize_commit(&commit),
            )
            .expect("commit");
        crate::refs::write_ref(&repo.git_dir(), "HEAD", &co).expect("head");
        ensure_worktree_clean_for_pick(&repo).expect("clean");
    }
}
