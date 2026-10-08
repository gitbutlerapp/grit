//! Lightweight worktree cleanliness checks without a full [`status`](super::status::status) pass.
//!
//! Pick, merge, and branch switch only need to know whether staged or unstaged changes exist
//! (and switch additionally needs untracked paths plus a tree-to-tree diff). This module loads
//! the index once, refreshes stat data when needed, and computes those slices without rename
//! detection, ahead/behind, stash counts, or in-progress operation state.

use crate::diff::{diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, DiffEntry};
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, Index};
use crate::objects::{parse_commit, ObjectId};
use crate::porcelain::status::{collect_untracked_and_ignored, IgnoredMode};
use crate::repo::Repository;
use crate::state::resolve_head;

/// Loaded index and HEAD tree for worktree mutation guards.
pub struct WorktreeSnapshot {
    /// Index with sparse-directory placeholders expanded.
    pub index: Index,
    /// Tree OID of the current HEAD commit, if any.
    pub head_tree: Option<ObjectId>,
}

/// Load the index (with stat refresh) and resolve HEAD's tree OID.
///
/// # Errors
///
/// Returns errors from index I/O, ODB reads, or when the repository has no work tree.
pub fn load_worktree_snapshot(repo: &Repository) -> Result<WorktreeSnapshot> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    let index_path = repo.index_path();
    let mut index = match Index::load(&index_path) {
        Ok(i) => i,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };
    let index_mtime = index.source_mtime;
    if crate::diff::refresh_index_stat_content_verified(
        &repo.odb,
        &repo.git_dir,
        &mut index,
        work_tree,
        index_mtime,
    ) && repo.try_write_index(&mut index)?
    {
        index.source_mtime = crate::index::index_file_mtime(&index_path);
    }
    let _ = index.expand_sparse_directory_placeholders(&repo.odb);

    let head = resolve_head(&repo.git_dir)?;
    let head_tree = match head.oid() {
        Some(oid) => {
            let obj = repo.odb.read(oid)?;
            Some(parse_commit(&obj.data)?.tree)
        }
        None => None,
    };

    Ok(WorktreeSnapshot { index, head_tree })
}

/// Staged and unstaged diffs for the current worktree (no rename detection).
fn local_change_diffs(
    repo: &Repository,
    snapshot: &WorktreeSnapshot,
) -> Result<(Vec<DiffEntry>, Vec<DiffEntry>)> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let index_path = repo.index_path();
    let index_mtime = index_file_mtime(&index_path);

    let staged = diff_index_to_tree(
        &repo.odb,
        &snapshot.index,
        snapshot.head_tree.as_ref(),
        false,
    )?;
    let unstaged = diff_index_to_worktree_with_options(
        &repo.odb,
        &snapshot.index,
        work_tree,
        crate::diff::DiffIndexToWorktreeOptions {
            index_mtime: crate::diff::index_mtime_for_diff(&snapshot.index, index_mtime),
            ignore_submodule_untracked: true,
            repository_git_dir: Some(repo.git_dir.clone()),
            ..Default::default()
        },
    )?;
    Ok((staged, unstaged))
}

fn ensure_worktree_clean_with_message(repo: &Repository, message: &str) -> Result<()> {
    let snapshot = load_worktree_snapshot(repo)?;
    let (staged, unstaged) = local_change_diffs(repo, &snapshot)?;
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
    let snapshot = load_worktree_snapshot(repo)?;
    let (staged, unstaged) = local_change_diffs(repo, &snapshot)?;
    if !staged.is_empty() || !unstaged.is_empty() {
        return Err(Error::Message(
            "you have uncommitted changes — commit them before switching".into(),
        ));
    }

    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;
    let (untracked, _) = collect_untracked_and_ignored(
        repo,
        &snapshot.index,
        work_tree,
        IgnoredMode::No,
        false,
        &[],
    )?;
    let tree_changes = diff_trees(&repo.odb, from_tree, Some(to_tree), "")?;
    Ok(TreeSwitchPlan {
        untracked,
        tree_changes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::repo::{init_repository, Repository};
    use tempfile::TempDir;

    fn init_repo() -> (TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, "files").expect("init");
        std::fs::write(dir.path().join("tracked.txt"), "v1\n").expect("write");
        (dir, repo)
    }

    #[test]
    fn ensure_worktree_clean_for_pick_passes_on_fresh_commit() {
        let (_dir, repo) = init_repo();
        let wt = repo.work_tree.as_ref().expect("wt");
        let mut index = repo.load_index().expect("index");
        let data = std::fs::read(wt.join("tracked.txt")).expect("read");
        let oid = repo
            .odb
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
            &repo.odb,
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
        };
        let co = repo
            .odb
            .write(
                crate::objects::ObjectKind::Commit,
                &crate::objects::serialize_commit(&commit),
            )
            .expect("commit");
        crate::refs::write_ref(&repo.git_dir, "HEAD", &co).expect("head");
        ensure_worktree_clean_for_pick(&repo).expect("clean");
    }
}
