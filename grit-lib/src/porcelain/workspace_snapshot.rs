//! Capture and restore repository workspace state for undo logs (index with conflict stages,
//! worktree bytes including conflict markers, `HEAD`, named refs, and in-progress operation sentinels).
//!
//! This is not a Git stash: stash commits require a merged index. Workspace snapshots preserve
//! unmerged index stages and record the worktree as it appears on disk.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::time::SystemTime;

use crate::error::{Error, Result};
use crate::index::{entry_from_stat, Index, MODE_GITLINK, MODE_TREE};
use crate::lockfile::LockWait;
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_between_trees_with_progress;
use crate::porcelain::stash::collect_untracked_paths;
use crate::progress::{Cancel, ProgressSink};
use crate::refs::prepare_with_lock_wait;
use crate::refs::store::{Expected, RawRef, RefTransaction, RefUpdate, RefUpdateFlags};
use crate::repo::Repository;
use crate::state::{
    merge_completion_sentinels, resolve_head_for_repository, HeadState, InProgressOperation,
};
use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

/// Options for [`capture_workspace_snapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceCaptureOptions {
    /// Ref names (for example `refs/heads/main`) whose [`RawRef`] values are stored.
    pub refs: Vec<String>,
    /// When true, untracked worktree paths are included in [`WorkspaceSnapshot::worktree_tree`].
    pub include_untracked: bool,
}

/// A point-in-time workspace snapshot suitable for undo/redo in front ends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceSnapshot {
    /// Resolved `HEAD` at capture time.
    pub head: HeadState,
    /// Caller-selected refs and their raw values (`None` when the ref did not exist).
    pub refs: BTreeMap<String, Option<RawRef>>,
    /// On-disk index bytes (including unmerged stages), as written by [`Index::write_to_path`].
    pub index_bytes: Vec<u8>,
    /// Index mtime when read from disk (preserved on restore for racy stat checks).
    pub index_mtime: Option<SystemTime>,
    /// Tree object describing worktree file contents at capture (conflict markers and untracked
    /// files when requested).
    pub worktree_tree: ObjectId,
    /// Git-dir sentinel files and directories (paths relative to `.git`).
    pub operation_state: BTreeMap<String, Vec<u8>>,
}

/// Options for [`restore_workspace_snapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceRestoreOptions {
    /// Reflog message for ref updates performed during restore (empty omits reflog entries).
    pub reflog_message: Option<String>,
}

/// Capture the current workspace without mutating the repository.
///
/// # Errors
///
/// Returns [`Error::RequiresWorkTree`] on a bare repository and propagates index, ODB, and I/O
/// failures.
pub fn capture_workspace_snapshot(
    repo: &Repository,
    options: &WorkspaceCaptureOptions,
) -> Result<WorkspaceSnapshot> {
    let head = resolve_head_for_repository(repo)?;
    let mut refs = BTreeMap::new();
    for name in &options.refs {
        let raw = repo.refs().read_raw(name)?;
        refs.insert(name.clone(), raw);
    }
    let (index_bytes, index_mtime) = capture_index_bytes(repo)?;
    let worktree_tree = build_worktree_tree(repo, options.include_untracked)?;
    let operation_state = capture_operation_state(repo.git_dir())?;
    Ok(WorkspaceSnapshot {
        head,
        refs,
        index_bytes,
        index_mtime,
        worktree_tree,
        operation_state,
    })
}

/// Restore a [`WorkspaceSnapshot`] in one coordinated step: refs and `HEAD`, index bytes,
/// worktree tree checkout, then operation sentinels.
///
/// # Errors
///
/// Propagates ref transaction, checkout, and I/O failures.
pub fn restore_workspace_snapshot(
    repo: &Repository,
    snapshot: &WorkspaceSnapshot,
    options: &WorkspaceRestoreOptions,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    commit_ref_restore(repo, snapshot, options)?;
    restore_index_bytes(repo, &snapshot.index_bytes, snapshot.index_mtime)?;
    restore_worktree_tree(repo, snapshot, progress, cancel)?;
    restore_index_bytes(repo, &snapshot.index_bytes, snapshot.index_mtime)?;
    restore_operation_state(repo.git_dir(), &snapshot.operation_state)?;
    Ok(())
}

/// Remove in-progress operation sentinels without moving `HEAD`, refs, or the worktree.
///
/// Returns the operations that were active before clearing (same order as
/// [`crate::state::detect_in_progress`]).
///
/// # Errors
///
/// Returns [`Error::Io`] when sentinel files cannot be removed.
pub fn clear_in_progress(repo: &Repository) -> Result<Vec<InProgressOperation>> {
    let git_dir = repo.git_dir();
    let ops = crate::state::detect_in_progress(git_dir);
    crate::state::clear_merge_state(git_dir)?;
    crate::state::clear_replay_state(git_dir)?;
    remove_dir_if_present(&git_dir.join("rebase-merge"))?;
    remove_dir_if_present(&git_dir.join("rebase-apply"))?;
    let journal = git_dir.join("grit/rewrite-apply");
    remove_dir_if_present(&journal)?;
    Ok(ops)
}

fn remove_dir_if_present(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(Error::Io)?;
    }
    Ok(())
}

fn capture_index_bytes(repo: &Repository) -> Result<(Vec<u8>, Option<SystemTime>)> {
    let path = repo.index_path();
    if path.is_file() {
        let mtime = fs::metadata(&path).and_then(|m| m.modified()).ok();
        return Ok((fs::read(&path).map_err(Error::Io)?, mtime));
    }
    let mut index = repo.load_index()?;
    let tmp = tempfile::NamedTempFile::new().map_err(Error::Io)?;
    index.write_to_path(tmp.path(), false)?;
    Ok((fs::read(tmp.path()).map_err(Error::Io)?, None))
}

fn restore_index_bytes(repo: &Repository, bytes: &[u8], mtime: Option<SystemTime>) -> Result<()> {
    let path = repo.index_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    fs::write(&path, bytes).map_err(Error::Io)?;
    if let Some(mtime) = mtime {
        fs::File::options()
            .write(true)
            .open(&path)
            .map_err(Error::Io)?
            .set_modified(mtime)
            .map_err(Error::Io)?;
    }
    Ok(())
}

fn build_worktree_tree(repo: &Repository, include_untracked: bool) -> Result<ObjectId> {
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let source = repo.load_index()?;
    let mut paths: BTreeSet<Vec<u8>> = source.entries.iter().map(|e| e.path.clone()).collect();
    if include_untracked {
        for path in collect_untracked_paths(repo, work_tree)? {
            paths.insert(path.into_bytes());
        }
    }
    let mut out = Index::new();
    let rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &source)?;
    let precompose = crate::precompose_config::effective_core_precomposeunicode_with_config(
        Some(repo.git_dir()),
        Some(&rules.config_arc()),
    );
    for path_bytes in paths {
        if let Some(entry) = source.get(&path_bytes, 0) {
            if entry.mode == MODE_GITLINK {
                out.add_or_replace(entry.clone());
                continue;
            }
        }
        let rel = crate::repo_relpath::storage_string(&path_bytes);
        let abs = crate::index::worktree_path_from_index_rel(work_tree, &path_bytes);
        if abs.symlink_metadata().is_ok() {
            stage_worktree_path_for_tree(
                repo,
                work_tree,
                &rel,
                &path_bytes,
                &mut out,
                precompose,
                &rules,
            )?;
        } else if let Some(entry) = source.get(&path_bytes, 0) {
            out.add_or_replace(entry.clone());
        } else if let Some(mut entry) = source
            .get(&path_bytes, 2)
            .or_else(|| source.get(&path_bytes, 3))
            .cloned()
        {
            entry.set_stage(0);
            out.add_or_replace(entry);
        }
    }
    out.sort();
    write_tree_update_index(repo.odb(), &mut out, "", WriteTreeFlags::silent())
}

fn stage_worktree_path_for_tree(
    repo: &Repository,
    work_tree: &Path,
    rel_path: &str,
    index_relpath: &[u8],
    index: &mut Index,
    precompose_unicode: bool,
    rules: &crate::worktree_rules::WorktreeRules,
) -> Result<()> {
    use crate::diff::mode_from_metadata;
    if rel_path.ends_with('/') {
        return Ok(());
    }
    let resolved = crate::unicode_normalization::resolve_worktree_path_for_staging(
        work_tree,
        rel_path,
        precompose_unicode,
    );
    let abs = resolved.abs;
    let index_path = resolved.index_relpath;
    let meta = fs::symlink_metadata(&abs).map_err(Error::Io)?;
    if meta.is_dir() {
        return Ok(());
    }
    let recorded_mode = index.get(index_path.as_bytes(), 0).map(|e| e.mode);
    let mode = crate::index::WorktreeModePolicy::from_config(rules.config())
        .index_mode_for_worktree(mode_from_metadata(&meta), recorded_mode);
    if mode == MODE_TREE {
        return Ok(());
    }
    let file_attrs = rules.file_attrs(&index_path, false);
    let conv = rules.conversion();
    let filter_fp = Some(rules.filter_process());
    let oid = if meta.file_type().is_symlink() {
        let target = fs::read_link(&abs).map_err(Error::Io)?;
        let data = crate::diff::symlink_target_bytes(&target);
        repo.odb().write(crate::objects::ObjectKind::Blob, &data)?
    } else {
        crate::diff::materialize_worktree_blob(
            repo.odb(),
            &abs,
            &meta,
            conv,
            &file_attrs,
            &index_path,
            None,
            filter_fp,
        )?
    };
    let entry = entry_from_stat(&abs, index_relpath, oid, mode)?;
    index.add_or_replace(entry);
    Ok(())
}

fn capture_operation_state(git_dir: &Path) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut out = BTreeMap::new();
    for name in merge_completion_sentinels() {
        capture_git_file(git_dir, name, &mut out)?;
    }
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD"] {
        capture_git_file(git_dir, name, &mut out)?;
    }
    for dir in [
        "sequencer",
        "rebase-merge",
        "rebase-apply",
        "grit/rewrite-apply",
    ] {
        capture_git_dir(git_dir, dir, &mut out)?;
    }
    Ok(out)
}

fn capture_git_file(git_dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let path = git_dir.join(rel);
    if path.is_file() {
        out.insert(rel.to_owned(), fs::read(&path).map_err(Error::Io)?);
    }
    Ok(())
}

fn capture_git_dir(git_dir: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    let root = git_dir.join(rel);
    if !root.is_dir() {
        return Ok(());
    }
    capture_dir_recursive(&root, rel, out)
}

fn capture_dir_recursive(abs: &Path, rel: &str, out: &mut BTreeMap<String, Vec<u8>>) -> Result<()> {
    for entry in fs::read_dir(abs).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let path = entry.path();
        if path.is_dir() {
            capture_dir_recursive(&path, &child_rel, out)?;
        } else if path.is_file() {
            out.insert(child_rel, fs::read(&path).map_err(Error::Io)?);
        }
    }
    Ok(())
}

fn restore_operation_state(git_dir: &Path, files: &BTreeMap<String, Vec<u8>>) -> Result<()> {
    for (rel, bytes) in files {
        let abs = git_dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        fs::write(&abs, bytes).map_err(Error::Io)?;
    }
    Ok(())
}

fn restore_worktree_tree(
    repo: &Repository,
    snapshot: &WorkspaceSnapshot,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    let head_oid = resolve_head_for_repository(repo)?
        .oid()
        .copied()
        .ok_or(Error::UnbornHead)?;
    let head_tree = repo.odb().commit_tree(&head_oid)?;
    checkout_between_trees_with_progress(
        repo,
        Some(&head_tree),
        &snapshot.worktree_tree,
        progress,
        cancel,
    )?;
    Ok(())
}

fn commit_ref_restore(
    repo: &Repository,
    snapshot: &WorkspaceSnapshot,
    _options: &WorkspaceRestoreOptions,
) -> Result<()> {
    let store = repo.refs();
    let config = repo.config().ok();
    let wait = LockWait::files_ref_lock(config.as_ref().map(|c| c.as_ref()));
    let mut txn = RefTransaction::new();
    for (name, new_value) in &snapshot.refs {
        let expected = match store.read_raw(name)? {
            Some(current) => expected_from_raw(&current),
            None => Expected::Missing,
        };
        let update = RefUpdate {
            name: name.clone(),
            new_value: new_value.clone(),
            expected,
            reflog: None,
            flags: Default::default(),
        };
        txn = txn.update(update).map_err(Error::from)?;
    }
    let head_update = head_ref_update(repo, snapshot)?;
    if let Some(update) = head_update {
        txn = txn.update(update).map_err(Error::from)?;
    }
    if txn.updates().is_empty() {
        return Ok(());
    }
    let prepared = prepare_with_lock_wait(store, txn, wait).map_err(Error::from)?;
    prepared.commit().map_err(Error::from)
}

fn expected_from_raw(raw: &RawRef) -> Expected {
    match raw {
        RawRef::Direct(oid) => Expected::Oid(*oid),
        RawRef::Symbolic(name) => Expected::Symref(name.clone()),
    }
}

fn head_ref_update(repo: &Repository, snapshot: &WorkspaceSnapshot) -> Result<Option<RefUpdate>> {
    let current = repo.refs().read_raw("HEAD")?;
    let new_value = match &snapshot.head {
        HeadState::Branch { refname, .. } => Some(RawRef::Symbolic(refname.clone())),
        HeadState::Detached { oid } => Some(RawRef::Direct(*oid)),
        HeadState::Invalid => None,
    };
    let Some(new_value) = new_value else {
        return Ok(None);
    };
    let expected = match current {
        Some(raw) => expected_from_raw(&raw),
        None => Expected::Missing,
    };
    Ok(Some(RefUpdate {
        name: "HEAD".to_owned(),
        new_value: Some(new_value),
        expected,
        reflog: None,
        flags: RefUpdateFlags {
            no_deref: true,
            ..Default::default()
        },
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::porcelain::merge::{integrate_branch, IntegrateBranchOutcome};
    use crate::progress::{NeverCancel, NullProgress};
    use grit_test_support::git;

    fn setup_merge_conflict() -> (tempfile::TempDir, Repository) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main"]);
        git(root, &["config", "core.autocrlf", "false"]);
        fs::write(root.join("a.txt"), "base\n").unwrap();
        git(root, &["add", "a.txt"]);
        git(root, &["commit", "-qm", "M1"]);
        git(root, &["switch", "-qc", "side"]);
        fs::write(root.join("a.txt"), "side\n").unwrap();
        git(root, &["commit", "-qam", "S1"]);
        git(root, &["switch", "-q", "main"]);
        fs::write(root.join("a.txt"), "main2\n").unwrap();
        git(root, &["commit", "-qam", "M2"]);
        let repo =
            Repository::discover(Some(root), &crate::environment::RepositoryOptions::empty())
                .unwrap();
        (tmp, repo)
    }

    #[test]
    fn capture_and_restore_preserves_unmerged_index() {
        let (_tmp, repo) = setup_merge_conflict();
        let root = repo.work_tree().unwrap();
        fs::write(root.join("dirty.txt"), "untracked\n").unwrap();
        let side = git(root, &["rev-parse", "side"]).trim().to_owned();
        let main = git(root, &["rev-parse", "main"]).trim().to_owned();
        let side_oid = ObjectId::from_hex(&side).unwrap();
        let main_oid = ObjectId::from_hex(&main).unwrap();
        let outcome = integrate_branch(
            &repo,
            "main",
            main_oid,
            side_oid,
            "side",
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert!(matches!(outcome, IntegrateBranchOutcome::Conflicts { .. }));

        let snap = capture_workspace_snapshot(
            &repo,
            &WorkspaceCaptureOptions {
                refs: vec!["refs/heads/main".into()],
                include_untracked: true,
            },
        )
        .unwrap();
        assert!(repo.git_dir().join("MERGE_HEAD").exists());

        git(root, &["reset", "--hard", "HEAD"]);
        assert!(!repo.git_dir().join("MERGE_HEAD").exists());

        restore_workspace_snapshot(
            &repo,
            &snap,
            &WorkspaceRestoreOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        assert!(repo.git_dir().join("MERGE_HEAD").exists());
        let ls = git(root, &["ls-files", "-u"]);
        assert!(ls.contains("a.txt"), "{ls}");
        assert_eq!(
            fs::read_to_string(root.join("dirty.txt")).unwrap(),
            "untracked\n"
        );
        git(root, &["fsck"]);
    }

    #[test]
    fn clear_in_progress_removes_merge_head_without_abort_checkout() {
        let (_tmp, repo) = setup_merge_conflict();
        let root = repo.work_tree().unwrap();
        let side = git(root, &["rev-parse", "side"]).trim().to_owned();
        let main = git(root, &["rev-parse", "main"]).trim().to_owned();
        integrate_branch(
            &repo,
            "main",
            ObjectId::from_hex(&main).unwrap(),
            ObjectId::from_hex(&side).unwrap(),
            "side",
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        let before = fs::read_to_string(root.join("a.txt")).unwrap();
        let cleared = clear_in_progress(&repo).unwrap();
        assert!(cleared.contains(&InProgressOperation::Merge));
        assert!(!repo.git_dir().join("MERGE_HEAD").exists());
        assert_eq!(fs::read_to_string(root.join("a.txt")).unwrap(), before);
    }
}
