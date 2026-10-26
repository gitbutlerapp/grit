//! Apply staged index changes onto a historical commit and replay descendants.

use std::fs;

use super::absorb::remaining_index_blob_for_path;
use super::apply::{apply_fixup_rewrite, apply_index_to_worktree_delta};
use super::error::HistoryError;
use super::plan::plan_rewrite;
use super::reword::normalize_rewrite_message;
use super::types::{AppliedRewrite, ApplyOptions};
use super::types::{CommitEdit, EmptyPolicy, HistoryEdits, RewriteOptions, RewritePlan};
use crate::apply::{apply_patch_to_blob, BlobApplyOptions};
use crate::diff::diff_index_to_tree;
use crate::diff::hunk_id::DiffSide;
use crate::index::write_tree::write_tree_from_index;
use crate::index::{Index, IndexEntry, MODE_REGULAR};
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_tree, CommitData, ObjectId, ObjectKind};
use crate::porcelain::hunk_select::{
    diff_options_for_hunk_select, resolve_selection, stage_hunks, HunkSelection,
};
use crate::porcelain::index_from_tree::index_from_tree;
use crate::repo::Repository;

/// Options for [`fixup`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixupOptions {
    /// Replacement message for the target commit; `None` keeps the existing message.
    pub message: Option<String>,
    /// How to handle the target (and replayed descendants) when a commit becomes empty.
    pub empty: EmptyPolicy,
    /// Ref scope, committer, and replay empty policy (the `empty` field is overwritten from [`Self::empty`]).
    pub rewrite: RewriteOptions,
    /// When set, fixup uses `HEAD`'s tree plus these unstaged hunks (in-memory index) instead of the full staged index.
    pub selection: Option<HunkSelection>,
    /// Pathspec filter when resolving [`Self::selection`].
    pub pathspecs: Vec<String>,
}

/// Plan a fixup of `target`: merge staged index changes onto `target`'s tree and replay descendants.
///
/// When [`FixupOptions::selection`] is set, only the selected unstaged hunks are folded; the on-disk
/// index is unchanged until [`apply_fixup_hunk_selection`].
///
/// Writes new commit objects into the ODB; refs are updated only when the caller applies the plan.
///
/// # Errors
///
/// Returns [`HistoryError::BareRepository`], [`HistoryError::NothingStaged`], merge
/// [`HistoryError::Conflict`], empty-policy failures, and other planning errors.
pub fn fixup(
    repo: &Repository,
    target: ObjectId,
    opts: &FixupOptions,
) -> Result<RewritePlan, HistoryError> {
    if repo.is_bare() {
        return Err(HistoryError::BareRepository);
    }

    let head_oid = repo.head_commit()?;
    let head_tree = commit_tree(repo, head_oid)?;
    let index_tree = if let Some(selection) = &opts.selection {
        index_tree_for_fixup_selection(repo, head_tree, selection, &opts.pathspecs)?
    } else {
        let index = repo.load_index().map_err(library_err)?;
        let staged =
            diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false).map_err(library_err)?;
        if staged.is_empty() {
            return Err(HistoryError::NothingStaged);
        }
        write_tree_from_index(repo.odb(), &index, "").map_err(library_err)?
    };
    let target_commit = load_commit(repo, target)?;
    let target_tree = target_commit.tree;

    record_fixup_merge();
    let merge = merge_trees_three_way(
        repo,
        head_tree,
        target_tree,
        index_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .map_err(library_err)?;

    if merge.index.has_unmerged_entries() || !merge.conflict_content.is_empty() {
        return Err(HistoryError::Conflict {
            commit: target,
            paths: crate::operation::tree_merge_conflict_path_strings(&merge),
        });
    }

    let merged_tree = merge.result_tree;
    let became_empty = fixup_target_became_empty(repo, &target_commit, merged_tree)?;

    let message = match &opts.message {
        Some(raw) => Some(normalize_rewrite_message(raw)?),
        None => None,
    };

    let mut edits = HistoryEdits::new();
    if became_empty {
        match opts.empty {
            EmptyPolicy::Abort => {
                return Err(HistoryError::FixupMakesTargetEmpty { commit: target });
            }
            EmptyPolicy::Drop => {
                edits.insert(target, CommitEdit::Drop);
            }
            EmptyPolicy::Keep => {
                edits.insert(
                    target,
                    CommitEdit::Rewrite {
                        tree: Some(merged_tree),
                        message,
                        author: None,
                        parents: None,
                    },
                );
            }
        }
    } else {
        edits.insert(
            target,
            CommitEdit::Rewrite {
                tree: Some(merged_tree),
                message,
                author: None,
                parents: None,
            },
        );
    }

    let mut rewrite_opts = opts.rewrite.clone();
    rewrite_opts.empty = opts.empty;
    plan_rewrite(repo, &edits, &rewrite_opts)
}

/// Repository-relative paths touched by a hunk selection (for [`apply_fixup_rewrite`]).
///
/// # Errors
///
/// Returns [`HistoryError::HunkSelect`] when ids or line ranges cannot be resolved.
pub fn fixup_selection_paths(
    repo: &Repository,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<Vec<String>, HistoryError> {
    let diff_opts = diff_options_for_hunk_select(repo).map_err(library_err)?;
    let patches = resolve_selection(repo, DiffSide::Unstaged, selection, pathspecs, diff_opts)?;
    let mut paths: Vec<String> = patches
        .iter()
        .filter_map(|p| p.effective_path().map(str::to_owned))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

/// Apply a fixup plan built with [`FixupOptions::selection`]: stage selected hunks, then run [`apply_fixup_rewrite`].
///
/// Pre-existing staged edits on the same path as a selected unstaged hunk are preserved in the
/// index and worktree after apply. On failure, the index is restored to its pre-call state.
///
/// # Errors
///
/// Same as [`apply_fixup_rewrite`], plus hunk resolution and staging failures.
pub fn apply_fixup_hunk_selection(
    repo: &Repository,
    plan: &RewritePlan,
    apply_opts: &ApplyOptions,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<AppliedRewrite, HistoryError> {
    let paths = fixup_selection_paths(repo, selection, pathspecs)?;
    let snapshot = capture_hunk_fixup_apply_snapshot(repo, &paths)?;
    let diff_opts = diff_options_for_hunk_select(repo).map_err(library_err)?;
    if let Err(err) = stage_hunks(
        repo,
        DiffSide::Unstaged,
        selection,
        pathspecs,
        diff_opts,
        false,
    ) {
        return Err(err.into());
    }
    match apply_fixup_rewrite(repo, plan, apply_opts, &paths) {
        Ok(applied) => {
            if let Err(err) = restore_after_hunk_fixup_apply(repo, &snapshot) {
                restore_full_hunk_fixup_snapshot(repo, &snapshot)?;
                return Err(err);
            }
            Ok(applied)
        }
        Err(err) => {
            restore_full_hunk_fixup_snapshot(repo, &snapshot)?;
            Err(err)
        }
    }
}

#[derive(Debug, Clone)]
struct HunkFixupPathSnapshot {
    path: String,
    pre_index_blob: Vec<u8>,
    pre_worktree_bytes: Option<Vec<u8>>,
    had_pre_staged: bool,
}

#[derive(Debug, Clone)]
struct HunkFixupApplySnapshot {
    index_bytes: Vec<u8>,
    pre_head_tree: ObjectId,
    paths: Vec<HunkFixupPathSnapshot>,
}

fn capture_hunk_fixup_apply_snapshot(
    repo: &Repository,
    paths: &[String],
) -> Result<HunkFixupApplySnapshot, HistoryError> {
    let index_bytes = fs::read(repo.index_path()).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    let pre_head_tree = commit_tree(repo, repo.head_commit()?)?;
    let index = repo.load_index().map_err(library_err)?;
    let mut path_states = Vec::with_capacity(paths.len());
    for path in paths {
        let pre_head_blob = blob_at_path(repo, pre_head_tree, path).unwrap_or_default();
        let pre_index_blob = index_blob_at_path(repo, &index, path);
        let had_pre_staged = pre_index_blob != pre_head_blob;
        let pre_worktree_bytes = repo.work_tree().as_ref().and_then(|wt| {
            let abs = wt.join(path);
            abs.is_file().then(|| fs::read(&abs).ok()).flatten()
        });
        path_states.push(HunkFixupPathSnapshot {
            path: path.clone(),
            pre_index_blob,
            pre_worktree_bytes,
            had_pre_staged,
        });
    }
    Ok(HunkFixupApplySnapshot {
        index_bytes,
        pre_head_tree,
        paths: path_states,
    })
}

fn restore_index_bytes(repo: &Repository, bytes: &[u8]) -> Result<(), HistoryError> {
    fs::write(repo.index_path(), bytes).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })
}

fn restore_full_hunk_fixup_snapshot(
    repo: &Repository,
    snapshot: &HunkFixupApplySnapshot,
) -> Result<(), HistoryError> {
    restore_index_bytes(repo, &snapshot.index_bytes)?;
    let Some(worktree) = repo.work_tree() else {
        return Ok(());
    };
    for path_state in &snapshot.paths {
        let abs = worktree.join(&path_state.path);
        match &path_state.pre_worktree_bytes {
            Some(bytes) => {
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).map_err(|e| HistoryError::WorktreeApply {
                        message: e.to_string(),
                    })?;
                }
                fs::write(&abs, bytes).map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
            }
            None if abs.exists() => {
                fs::remove_file(&abs).map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
            }
            None => {}
        }
    }
    Ok(())
}

fn restore_after_hunk_fixup_apply(
    repo: &Repository,
    snapshot: &HunkFixupApplySnapshot,
) -> Result<(), HistoryError> {
    let post_head_tree = commit_tree(repo, repo.head_commit()?)?;
    let mut index = repo.load_index().map_err(library_err)?;
    let mut index_changed = false;
    let worktree = repo.work_tree();
    for path_state in &snapshot.paths {
        if !path_state.had_pre_staged {
            continue;
        }
        let new_index_blob = remaining_index_blob_for_path(
            repo,
            snapshot.pre_head_tree,
            post_head_tree,
            &path_state.pre_index_blob,
            &path_state.path,
            &[],
        )?;
        upsert_index_blob(repo, &mut index, &path_state.path, &new_index_blob)?;
        index_changed = true;
        if let Some(wt) = worktree.as_ref() {
            let pre_worktree = path_state
                .pre_worktree_bytes
                .as_deref()
                .unwrap_or(&path_state.pre_index_blob);
            let new_worktree = apply_index_to_worktree_delta(
                &path_state.pre_index_blob,
                pre_worktree,
                &new_index_blob,
            );
            let abs = wt.join(&path_state.path);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent).map_err(|e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                })?;
            }
            fs::write(&abs, &new_worktree).map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
        }
    }
    if index_changed {
        index.sort();
        repo.write_index(&mut index)
            .map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
    }
    Ok(())
}

fn index_blob_at_path(repo: &Repository, index: &Index, path: &str) -> Vec<u8> {
    index
        .get(path.as_bytes(), 0)
        .and_then(|entry| repo.odb().read(&entry.oid).ok().map(|raw| raw.data))
        .unwrap_or_default()
}

fn upsert_index_blob(
    repo: &Repository,
    index: &mut Index,
    path: &str,
    bytes: &[u8],
) -> Result<(), HistoryError> {
    let oid = repo
        .odb()
        .write(ObjectKind::Blob, bytes)
        .map_err(library_err)?;
    let key = path.as_bytes();
    if let Some(entry) = index.get_mut(key, 0) {
        entry.oid = oid;
        entry.size = bytes.len() as u32;
    } else {
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: bytes.len() as u32,
            oid,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: key.to_vec(),
            base_index_pos: 0,
        });
    }
    Ok(())
}

fn blob_at_path(repo: &Repository, tree: ObjectId, path: &str) -> Result<Vec<u8>, HistoryError> {
    let mut current = tree;
    let parts: Vec<&str> = path.split('/').collect();
    for (idx, part) in parts.iter().enumerate() {
        let raw = repo.odb().read(&current).map_err(library_err)?;
        let entries = parse_tree(&raw.data).map_err(library_err)?;
        let Some(entry) = entries
            .iter()
            .find(|e| e.name.as_slice() == part.as_bytes())
        else {
            return Err(HistoryError::Planning {
                message: format!("path '{path}' not found in tree"),
            });
        };
        if idx + 1 == parts.len() {
            let blob = repo.odb().read(&entry.oid).map_err(library_err)?;
            return Ok(blob.data);
        }
        current = entry.oid;
    }
    Err(HistoryError::Planning {
        message: format!("path '{path}' not found in tree"),
    })
}

/// Build the blob folded into the fixup target: `HEAD` plus selected unstaged edits only.
///
/// Selection patches are resolved against the index→worktree unstaged diff, applied to the
/// live index blob, then replayed onto `HEAD` so pre-staged lines are not folded in.
fn planning_blob_for_fixup_selection(
    head_blob: &[u8],
    index_blob: &[u8],
    after_index_blob: &[u8],
) -> Result<Vec<u8>, HistoryError> {
    Ok(apply_index_to_worktree_delta(
        index_blob,
        after_index_blob,
        head_blob,
    ))
}

fn index_tree_for_fixup_selection(
    repo: &Repository,
    head_tree: ObjectId,
    selection: &HunkSelection,
    pathspecs: &[String],
) -> Result<ObjectId, HistoryError> {
    let diff_opts = diff_options_for_hunk_select(repo).map_err(library_err)?;
    let patches = resolve_selection(repo, DiffSide::Unstaged, selection, pathspecs, diff_opts)
        .map_err(HistoryError::HunkSelect)?;
    if patches.is_empty() {
        return Err(HistoryError::NothingStaged);
    }
    let disk_index = repo.load_index().map_err(library_err)?;
    let mut index = index_from_tree(repo, &head_tree).map_err(library_err)?;
    for patch in &patches {
        let path = patch
            .effective_path()
            .ok_or_else(|| HistoryError::Planning {
                message: "fixup selection patch missing path".into(),
            })?;
        let head_blob = blob_at_path(repo, head_tree, path)?;
        let index_blob = index_blob_at_path(repo, &disk_index, path);
        let after_index = apply_patch_to_blob(&index_blob, patch, &BlobApplyOptions::default())
            .map_err(|e| HistoryError::Planning {
                message: e.to_string(),
            })?;
        let planning_blob =
            planning_blob_for_fixup_selection(&head_blob, &index_blob, &after_index)?;
        upsert_index_blob(repo, &mut index, path, &planning_blob)?;
    }
    write_tree_from_index(repo.odb(), &index, "").map_err(library_err)
}

fn library_err(err: crate::error::Error) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
    }
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    repo.odb().read_commit(&oid).map_err(library_err)
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, HistoryError> {
    repo.odb().commit_tree(&commit).map_err(library_err)
}

fn empty_tree_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    repo.odb().empty_tree_oid().map_err(library_err)
}

fn first_parent_tree(repo: &Repository, commit: &CommitData) -> Result<ObjectId, HistoryError> {
    let empty = empty_tree_oid(repo)?;
    if commit.parents.is_empty() {
        return Ok(empty);
    }
    commit_tree(repo, commit.parents[0])
}

fn fixup_target_became_empty(
    repo: &Repository,
    target: &CommitData,
    merged_tree: ObjectId,
) -> Result<bool, HistoryError> {
    let parent_tree = first_parent_tree(repo, target)?;
    Ok(merged_tree == parent_tree)
}

#[cfg(any(test, feature = "unstable-bench-helpers"))]
static FIXUP_MERGE_CALLS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0); // hygiene: merge-call counter for tests and unstable-bench-helpers

fn record_fixup_merge() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    FIXUP_MERGE_CALLS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Repository-relative paths that differ between the index and `HEAD`'s tree (stage 0).
///
/// # Errors
///
/// Returns [`HistoryError::BareRepository`] or planning failures when the index or `HEAD` cannot be read.
pub fn staged_fixup_paths(repo: &Repository) -> Result<Vec<String>, HistoryError> {
    if repo.is_bare() {
        return Err(HistoryError::BareRepository);
    }
    let head_oid = repo.head_commit()?;
    let head_tree = commit_tree(repo, head_oid)?;
    let index = repo.load_index().map_err(library_err)?;
    let staged =
        diff_index_to_tree(repo.odb(), &index, Some(&head_tree), false).map_err(library_err)?;
    let mut paths: Vec<String> = staged
        .into_iter()
        .filter_map(|e| e.new_path.or(e.old_path))
        .collect();
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[cfg(test)]
mod hunk_selection_restore_tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::diff::hunk_id::hunk_ids_with_duplicate_peers;
    use crate::diff::DiffOptions;
    use crate::history::RefScope;
    use crate::objects::ObjectId;
    use crate::porcelain::hunk_select::{unstaged_file_diffs, LineRange};
    use crate::repo::{init_repository, set_test_inject_index_write_fail_remaining};
    use crate::RefStorageFormat;
    use std::process::Command;

    fn git(dir: &std::path::Path, args: &[&str]) {
        let out = Command::new("git")
            .current_dir(dir)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn two_hunk_edited_file() -> (String, String) {
        let mut base = String::new();
        for i in 0..24 {
            base.push_str(&format!("line{i}\n"));
        }
        let mut edited = base.clone();
        edited = edited.replacen("line5\n", "LINE5\n", 1);
        edited = edited.replacen("line18\n", "LINE18\n", 1);
        (base, edited)
    }

    fn committer() -> String {
        "Fixup <f@grit.test> 1 +0000".into()
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn restore_after_apply_failure_keeps_pre_call_index_and_worktree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let wt = dir.path();
        init_repository(wt, false, "main", None, RefStorageFormat::Files).expect("init");
        let (base, edited) = two_hunk_edited_file();
        fs::write(wt.join("f.txt"), &base).expect("write");
        git(wt, &["add", "f.txt"]);
        git(
            wt,
            &["commit", "-m", "init", "--author", "Fixup <f@grit.test>"],
        );
        git(wt, &["commit", "--allow-empty", "-m", "second"]);
        let target = {
            let out = Command::new("git")
                .current_dir(wt)
                .args(["rev-parse", "HEAD~1"])
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .expect("rev-parse");
            ObjectId::from_hex(String::from_utf8_lossy(&out.stdout).trim()).expect("oid")
        };
        fs::write(wt.join("f.txt"), &edited).expect("edit");
        let grit = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("discover");
        let files = unstaged_file_diffs(&grit, &[]).expect("diff");
        let ids = hunk_ids_with_duplicate_peers(
            DiffSide::Unstaged,
            &files[0].0.path,
            &files[0].0.hunks,
            &files[0].1,
        );
        stage_hunks(
            &grit,
            DiffSide::Unstaged,
            &HunkSelection {
                ids: vec![ids[0].clone()],
                ranges: Vec::new(),
            },
            &[],
            DiffOptions::default(),
            false,
        )
        .expect("stage line-5");
        let selection = HunkSelection {
            ids: Vec::new(),
            ranges: vec![LineRange {
                path: "f.txt".into(),
                start: 19,
                end: 19,
            }],
        };
        let pre_index = fs::read(wt.join(".git/index")).expect("index");
        let pre_worktree = fs::read(wt.join("f.txt")).expect("worktree");
        let head_before = grit.head_commit().expect("head");
        let opts = FixupOptions {
            message: None,
            empty: EmptyPolicy::Drop,
            rewrite: RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: committer().parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
            selection: Some(selection.clone()),
            pathspecs: Vec::new(),
        };
        let plan = fixup(&grit, target, &opts).expect("plan");
        let paths = fixup_selection_paths(&grit, &selection, &[]).expect("paths");
        let snapshot = capture_hunk_fixup_apply_snapshot(&grit, &paths).expect("snap");
        stage_hunks(
            &grit,
            DiffSide::Unstaged,
            &selection,
            &[],
            DiffOptions::default(),
            false,
        )
        .expect("stage line-18");
        let apply_opts = ApplyOptions {
            identity: "Fixup <f@grit.test> 1 +0000".parse().expect("identity"),
            reflog_message: "history fixup".into(),
            record_operation: None,
            hooks: crate::hooks::PostOperationHooks::Run,
        };
        apply_fixup_rewrite(&grit, &plan, &apply_opts, &paths).expect("apply rewrite");
        set_test_inject_index_write_fail_remaining(1);
        let err = restore_after_hunk_fixup_apply(&grit, &snapshot).expect_err("restore fails");
        restore_full_hunk_fixup_snapshot(&grit, &snapshot).expect("rollback snapshot");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(err, HistoryError::WorktreeApply { .. }));
        assert_ne!(
            grit.head_commit().expect("head after"),
            head_before,
            "HEAD should move during apply before restore failed"
        );
        assert_eq!(fs::read(wt.join(".git/index")).expect("index"), pre_index);
        assert_eq!(fs::read(wt.join("f.txt")).expect("worktree"), pre_worktree);
        git(wt, &["fsck", "--strict"]);
    }
}
