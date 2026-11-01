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
use crate::path_validate::ProtectOptions;
use crate::porcelain::checkout::checkout_between_trees_with_progress;
use crate::porcelain::stash::{collect_untracked_paths, flatten_tree_full};
use crate::progress::{Cancel, ProgressSink};
use crate::refs::prepare_with_lock_wait;
use crate::refs::store::{Expected, RawRef, RefName, RefTransaction, RefUpdate, RefUpdateFlags};
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

/// Ref prefix for pins created by [`pin_workspace_snapshot`].
///
/// Each pin is `refs/grit/workspace-snapshots/<caller-id>` pointing at
/// [`WorkspaceSnapshot::worktree_tree`]. Such refs are not under `refs/heads/`, so a tree oid is
/// allowed and both Git and grit garbage collection retain the tree and reachable blobs.
pub const WORKSPACE_SNAPSHOT_PIN_REF_PREFIX: &str = "refs/grit/workspace-snapshots/";

/// A point-in-time workspace snapshot suitable for undo/redo in front ends.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize, serde::Deserialize))]
pub struct WorkspaceSnapshot {
    /// Resolved `HEAD` at capture time.
    pub head: HeadState,
    /// Caller-selected refs and their raw values (`None` when the ref did not exist).
    pub refs: BTreeMap<String, Option<RawRef>>,
    /// On-disk index bytes (including unmerged stages), as written by [`Index::write_to_path`].
    pub index_bytes: Vec<u8>,
    /// Index mtime when read from disk (preserved on restore for racy stat checks).
    #[cfg_attr(feature = "serde-models", serde(with = "index_mtime_serde"))]
    pub index_mtime: Option<SystemTime>,
    /// Tree object describing worktree file contents at capture (conflict markers and untracked
    /// files when requested).
    pub worktree_tree: ObjectId,
    /// Git-dir sentinel files and directories (paths relative to `.git`).
    pub operation_state: BTreeMap<String, Vec<u8>>,
}

/// Options for [`restore_workspace_snapshot`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceRestoreOptions {}

/// Full ref name that pins `snapshot.worktree_tree` for `caller_id`.
///
/// # Errors
///
/// Returns [`Error::InvalidRef`] when `caller_id` produces an invalid refname.
pub fn workspace_snapshot_pin_ref_name(caller_id: &str) -> Result<String> {
    let name = format!("{WORKSPACE_SNAPSHOT_PIN_REF_PREFIX}{caller_id}");
    RefName::try_from(name.as_str()).map_err(|err| Error::InvalidRef(err.to_string()))?;
    Ok(name)
}

/// Write a direct ref at [`workspace_snapshot_pin_ref_name`] so `snapshot.worktree_tree` and its
/// blobs survive garbage collection until [`unpin_workspace_snapshot`].
///
/// [`capture_workspace_snapshot`] does not pin; call this when the snapshot will be stored for a
/// later process (for example an undo log entry).
///
/// # Errors
///
/// Propagates invalid refname and ref-transaction failures.
pub fn pin_workspace_snapshot(
    repo: &Repository,
    snapshot: &WorkspaceSnapshot,
    caller_id: &str,
) -> Result<()> {
    let name = workspace_snapshot_pin_ref_name(caller_id)?;
    let update = RefUpdate {
        name,
        new_value: Some(RawRef::Direct(snapshot.worktree_tree)),
        expected: Expected::Any,
        reflog: None,
        flags: Default::default(),
    };
    let store = repo.refs();
    let config = repo.config().ok();
    let wait = LockWait::files_ref_lock(config.as_ref().map(|c| c.as_ref()));
    let txn = RefTransaction::new().update(update).map_err(Error::from)?;
    let prepared = prepare_with_lock_wait(store, txn, wait).map_err(Error::from)?;
    prepared.commit().map_err(Error::from)
}

/// Delete the pin ref created for `caller_id`.
///
/// # Errors
///
/// Propagates invalid refname and ref-transaction failures.
pub fn unpin_workspace_snapshot(repo: &Repository, caller_id: &str) -> Result<()> {
    let name = workspace_snapshot_pin_ref_name(caller_id)?;
    let update = RefUpdate {
        name,
        new_value: None,
        expected: Expected::Any,
        reflog: None,
        flags: Default::default(),
    };
    let store = repo.refs();
    let config = repo.config().ok();
    let wait = LockWait::files_ref_lock(config.as_ref().map(|c| c.as_ref()));
    let txn = RefTransaction::new().update(update).map_err(Error::from)?;
    let prepared = prepare_with_lock_wait(store, txn, wait).map_err(Error::from)?;
    prepared.commit().map_err(Error::from)
}

#[cfg(feature = "serde-models")]
mod index_mtime_serde {
    use serde::{Deserialize, Deserializer, Serialize, Serializer};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Serialize, Deserialize)]
    struct MtimeFields {
        secs: u64,
        nanos: u32,
    }

    pub fn serialize<S>(mtime: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match mtime {
            None => serializer.serialize_none(),
            Some(t) => {
                let duration = t
                    .duration_since(UNIX_EPOCH)
                    .map_err(serde::ser::Error::custom)?;
                serializer.serialize_some(&MtimeFields {
                    secs: duration.as_secs(),
                    nanos: duration.subsec_nanos(),
                })
            }
        }
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<SystemTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        let fields: Option<MtimeFields> = Option::deserialize(deserializer)?;
        Ok(fields.map(|f| UNIX_EPOCH + Duration::new(f.secs, f.nanos)))
    }
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
/// Worktree checkout diffs the on-disk tree at restore time against the snapshot worktree
/// tree (not the post-restore `HEAD` tree), so undoing a branch switch or local deletions
/// recreates or removes paths even when they matched `HEAD` at capture.
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
    let pre_restore_index = repo.load_index()?;
    let disk_tree = build_pre_restore_disk_tree(repo, &pre_restore_index, &snapshot.worktree_tree)?;
    commit_ref_restore(repo, snapshot, options)?;
    restore_index_bytes(repo, &snapshot.index_bytes, snapshot.index_mtime)?;
    restore_worktree_tree(repo, &disk_tree, snapshot, progress, cancel)?;
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
    clear_managed_operation_sentinels(git_dir)?;
    Ok(ops)
}

/// Remove every operation sentinel path [`capture_operation_state`] may record.
pub(crate) fn clear_managed_operation_sentinels(git_dir: &Path) -> Result<()> {
    crate::state::clear_merge_state(git_dir)?;
    crate::state::clear_replay_state(git_dir)?;
    for dir in [
        "sequencer",
        "rebase-merge",
        "rebase-apply",
        "grit/rewrite-apply",
    ] {
        remove_dir_if_present(&git_dir.join(dir))?;
    }
    Ok(())
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
    clear_managed_operation_sentinels(git_dir)?;
    for (rel, bytes) in files {
        let abs = git_dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).map_err(Error::Io)?;
        }
        fs::write(&abs, bytes).map_err(Error::Io)?;
    }
    Ok(())
}

fn build_pre_restore_disk_tree(
    repo: &Repository,
    source: &Index,
    snapshot_worktree_tree: &ObjectId,
) -> Result<ObjectId> {
    let work_tree = repo.work_tree().ok_or(Error::RequiresWorkTree)?;
    let protect = ProtectOptions::default();
    let mut paths: BTreeSet<Vec<u8>> = source.entries.iter().map(|e| e.path.clone()).collect();
    for entry in flatten_tree_full(repo.odb(), snapshot_worktree_tree, "", protect)? {
        paths.insert(entry.path.into_bytes());
    }
    let mut out = Index::new();
    let rules = crate::worktree_rules::WorktreeRules::from_repository(repo, source)?;
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
        }
    }
    out.sort();
    write_tree_update_index(repo.odb(), &mut out, "", WriteTreeFlags::silent())
}

fn restore_worktree_tree(
    repo: &Repository,
    disk_tree: &ObjectId,
    snapshot: &WorkspaceSnapshot,
    progress: &mut dyn ProgressSink,
    cancel: &dyn Cancel,
) -> Result<()> {
    checkout_between_trees_with_progress(
        repo,
        Some(disk_tree),
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
    use crate::porcelain::switch::{resolve_switch_target, switch_repository, SwitchOptions};
    use crate::progress::{NeverCancel, NullProgress};
    use grit_test_support::{git, git_cmd};

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

    #[test]
    fn restore_after_switch_removes_branch_only_files() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), "one\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);
        git(root, &["switch", "-qc", "feat"]);
        fs::create_dir(root.join("d")).unwrap();
        fs::write(root.join("d/f.txt"), "f\n").unwrap();
        fs::write(root.join("g.txt"), "g\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "feat"]);
        git(root, &["switch", "-q", "main"]);

        let repo =
            Repository::discover(Some(root), &crate::environment::RepositoryOptions::empty())
                .unwrap();
        let snap = capture_workspace_snapshot(
            &repo,
            &WorkspaceCaptureOptions {
                refs: vec!["refs/heads/main".into(), "refs/heads/feat".into()],
                include_untracked: true,
            },
        )
        .unwrap();

        let mode = resolve_switch_target(&repo, "feat").unwrap();
        switch_repository(
            &repo,
            &SwitchOptions {
                mode,
                dry_run: false,
                hooks: Default::default(),
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert!(root.join("d").is_dir());
        assert!(root.join("g.txt").is_file());

        restore_workspace_snapshot(
            &repo,
            &snap,
            &WorkspaceRestoreOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        assert_eq!(git(root, &["branch", "--show-current"]).trim(), "main");
        assert!(root.join("a.txt").is_file());
        assert!(!root.join("d").exists());
        assert!(!root.join("g.txt").exists());
        assert!(git(root, &["status", "--short"]).trim().is_empty());
        git(root, &["fsck"]);
    }

    #[test]
    fn restore_recreates_tracked_file_deleted_after_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("keep.txt"), "keep\n").unwrap();
        fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);

        let repo =
            Repository::discover(Some(root), &crate::environment::RepositoryOptions::empty())
                .unwrap();
        let snap = capture_workspace_snapshot(
            &repo,
            &WorkspaceCaptureOptions {
                refs: vec!["refs/heads/main".into()],
                include_untracked: false,
            },
        )
        .unwrap();

        fs::remove_file(root.join("run.sh")).unwrap();
        assert!(!root.join("run.sh").exists());

        restore_workspace_snapshot(
            &repo,
            &snap,
            &WorkspaceRestoreOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        assert_eq!(
            fs::read_to_string(root.join("run.sh")).unwrap(),
            "#!/bin/sh\n"
        );
        assert!(git(root, &["status", "--short"]).trim().is_empty());
    }

    #[test]
    fn restore_removes_file_added_after_capture() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        git(root, &["init", "-q", "-b", "main"]);
        fs::write(root.join("a.txt"), "a\n").unwrap();
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "base"]);

        let repo =
            Repository::discover(Some(root), &crate::environment::RepositoryOptions::empty())
                .unwrap();
        let snap = capture_workspace_snapshot(
            &repo,
            &WorkspaceCaptureOptions {
                refs: vec!["refs/heads/main".into()],
                include_untracked: true,
            },
        )
        .unwrap();

        fs::write(root.join("h.txt"), "new\n").unwrap();
        git(root, &["add", "h.txt"]);
        git(root, &["commit", "-qm", "add h"]);

        restore_workspace_snapshot(
            &repo,
            &snap,
            &WorkspaceRestoreOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();

        assert!(!root.join("h.txt").exists());
        assert!(git(root, &["status", "--short"]).trim().is_empty());
        assert_eq!(git(root, &["rev-list", "--count", "HEAD"]).trim(), "1");
    }

    #[test]
    fn restore_clears_later_operation_sentinels_not_in_snapshot() {
        let (_tmp, repo) = setup_merge_conflict();
        let root = repo.work_tree().unwrap();
        let snap = capture_workspace_snapshot(
            &repo,
            &WorkspaceCaptureOptions {
                refs: vec![],
                include_untracked: false,
            },
        )
        .unwrap();
        assert!(snap.operation_state.is_empty());
        assert!(crate::state::detect_in_progress(repo.git_dir()).is_empty());

        fs::write(
            repo.git_dir().join("MERGE_HEAD"),
            git(root, &["rev-parse", "HEAD"]),
        )
        .unwrap();
        assert_eq!(
            crate::state::detect_in_progress(repo.git_dir()),
            vec![InProgressOperation::Merge]
        );

        restore_workspace_snapshot(
            &repo,
            &snap,
            &WorkspaceRestoreOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert!(crate::state::detect_in_progress(repo.git_dir()).is_empty());
        assert!(!repo.git_dir().join("MERGE_HEAD").exists());
    }

    fn capture_conflict_snapshot_with_untracked(repo: &Repository) -> WorkspaceSnapshot {
        let root = repo.work_tree().unwrap();
        fs::write(root.join("dirty.txt"), "untracked-bytes\n").unwrap();
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
        capture_workspace_snapshot(
            repo,
            &WorkspaceCaptureOptions {
                refs: vec!["refs/heads/main".into()],
                include_untracked: true,
            },
        )
        .unwrap()
    }

    #[test]
    fn unpinned_worktree_tree_is_pruned_by_git_gc() {
        let (_tmp, repo) = setup_merge_conflict();
        let root = repo.work_tree().unwrap();
        let snap = capture_conflict_snapshot_with_untracked(&repo);
        let tree_hex = snap.worktree_tree.to_hex();
        git(root, &["gc", "--prune=now", "-q"]);
        let _ = git_cmd(&["cat-file", "-t", &tree_hex])
            .in_dir(root)
            .with_status(128);
    }

    #[test]
    fn pinned_worktree_tree_survives_git_gc_and_grit_prune() {
        use crate::maintenance::collect_maintenance_object_roots;
        use crate::refs::gc::prune_loose_unreachable;

        let (_tmp, repo) = setup_merge_conflict();
        let root = repo.work_tree().unwrap();
        let snap = capture_conflict_snapshot_with_untracked(&repo);
        pin_workspace_snapshot(&repo, &snap, "undo-1").unwrap();
        let tree_hex = snap.worktree_tree.to_hex();

        git(root, &["gc", "--prune=now", "-q"]);
        assert_eq!(git(root, &["cat-file", "-t", &tree_hex]).trim(), "tree");

        let roots = collect_maintenance_object_roots(&repo).unwrap();
        prune_loose_unreachable(repo.odb(), &roots, None).unwrap();
        assert_eq!(git(root, &["cat-file", "-t", &tree_hex]).trim(), "tree");

        restore_workspace_snapshot(
            &repo,
            &snap,
            &WorkspaceRestoreOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert_eq!(
            fs::read_to_string(root.join("dirty.txt")).unwrap(),
            "untracked-bytes\n"
        );
    }

    #[test]
    fn unpin_allows_worktree_tree_to_be_pruned() {
        let (_tmp, repo) = setup_merge_conflict();
        let root = repo.work_tree().unwrap();
        let snap = capture_conflict_snapshot_with_untracked(&repo);
        pin_workspace_snapshot(&repo, &snap, "undo-1").unwrap();
        unpin_workspace_snapshot(&repo, "undo-1").unwrap();
        let tree_hex = snap.worktree_tree.to_hex();
        git(root, &["gc", "--prune=now", "-q"]);
        let _ = git_cmd(&["cat-file", "-t", &tree_hex])
            .in_dir(root)
            .with_status(128);
    }

    #[cfg(feature = "serde-models")]
    #[test]
    fn workspace_snapshot_serde_round_trip() {
        let (_tmp, repo) = setup_merge_conflict();
        let snap = snapshot_with_direct_and_symbolic_refs(&repo);
        let bytes = serde_json::to_vec(&snap).expect("serialize");
        let decoded: WorkspaceSnapshot = serde_json::from_slice(&bytes).expect("deserialize");
        assert_eq!(decoded, snap);
    }

    #[cfg(feature = "serde-models")]
    fn snapshot_with_direct_and_symbolic_refs(repo: &Repository) -> WorkspaceSnapshot {
        let mut snap = capture_conflict_snapshot_with_untracked(repo);
        snap.refs.insert(
            "HEAD".into(),
            Some(RawRef::Symbolic("refs/heads/main".into())),
        );
        snap
    }

    #[cfg(feature = "serde-models")]
    #[test]
    fn workspace_snapshot_bincode_round_trip() {
        use bincode::config;

        let (_tmp, repo) = setup_merge_conflict();
        let snap = snapshot_with_direct_and_symbolic_refs(&repo);
        assert!(matches!(
            snap.refs.get("refs/heads/main").and_then(|r| r.as_ref()),
            Some(RawRef::Direct(_))
        ));
        assert!(matches!(
            snap.refs.get("HEAD").and_then(|r| r.as_ref()),
            Some(RawRef::Symbolic(_))
        ));

        let cfg = config::standard();
        let bytes = bincode::serde::encode_to_vec(&snap, cfg).expect("bincode encode");
        let (decoded, len): (WorkspaceSnapshot, usize) =
            bincode::serde::decode_from_slice(&bytes, cfg).expect("bincode decode");
        assert_eq!(len, bytes.len());
        assert_eq!(decoded, snap);
    }
}
