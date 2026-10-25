//! Crash-safe journal for [`super::apply::apply_rewrite`] and related apply paths.
//!
//! When a rewrite updates refs before the worktree checkout finishes, a kill can leave refs at
//! the new tips while the index and worktree still reflect the old tree. A pending journal under
//! `.git/grit/rewrite-apply/` records inverse ref movements and pre-apply index/worktree
//! snapshots so recovery can finish or roll back the operation.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::lockfile::{LockFile, LockWait};
use crate::objects::ObjectId;
use crate::refs::resolve_ref;
use crate::repo::Repository;

use super::{
    apply_head_tree_to_worktree, restore_worktree_after_failed_apply, rollback_ref_updates,
    WorktreePathKind, WorktreePathSnapshot,
};
use crate::history::error::HistoryError;
use crate::history::types::{ApplyOptions, HeadEffect, PlannedRefUpdate, RewritePlan};

const FORMAT_VERSION: u32 = 1;
const META: &str = "meta";
const INDEX_BEFORE: &str = "index-before";
const WORKTREE_DIR: &str = "worktree";

/// Outcome of [`recover_interrupted_rewrite_apply`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RewriteApplyRecovery {
    /// Ref tips already matched the planned result; the worktree checkout was completed.
    FinishedCheckout,
    /// Ref tips were rolled back and index/worktree snapshots were restored.
    RolledBack,
}

/// Whether `.git/grit/rewrite-apply/` records an incomplete rewrite apply.
#[must_use]
pub fn rewrite_apply_journal_active(git_dir: &Path) -> bool {
    journal_dir(git_dir).join(META).is_file()
}

/// Persist a pending journal before ref updates when a worktree transition is required.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordWrite`] on I/O failures.
pub fn write_rewrite_apply_journal(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
    index_before: &[u8],
    worktree_before: &HashMap<String, WorktreePathSnapshot>,
) -> Result<(), HistoryError> {
    let dir = journal_dir(repo.git_dir());
    fs::create_dir_all(&dir).map_err(journal_write_err)?;
    let worktree_dir = dir.join(WORKTREE_DIR);
    if worktree_dir.exists() {
        fs::remove_dir_all(&worktree_dir).map_err(journal_write_err)?;
    }
    fs::create_dir_all(&worktree_dir).map_err(journal_write_err)?;

    fs::write(dir.join(INDEX_BEFORE), index_before).map_err(journal_write_err)?;
    for (path, snap) in worktree_before {
        write_worktree_snapshot_file(&worktree_dir, path, snap)?;
    }

    let body = serialize_meta(plan, opts);
    let lock_path = dir.join(format!("{META}.lock"));
    let target = dir.join(META);
    let mut lock =
        LockFile::acquire(&lock_path, LockWait::Immediately).map_err(journal_write_err)?;
    lock.write_all(body.as_bytes()).map_err(journal_write_err)?;
    lock.commit_to(&target).map_err(journal_write_err)?;
    Ok(())
}

/// Remove a completed or recovered journal.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordWrite`] when the directory cannot be removed.
pub fn clear_rewrite_apply_journal(repo: &Repository) -> Result<(), HistoryError> {
    let dir = journal_dir(repo.git_dir());
    if dir.is_dir() {
        fs::remove_dir_all(&dir).map_err(journal_write_err)?;
    }
    Ok(())
}

/// Finish or roll back an interrupted rewrite apply recorded in the journal.
///
/// When every planned ref already points at its new OID, completes the pending worktree checkout.
/// When every ref still matches the pre-apply OID, clears the journal. Otherwise rolls refs back
/// and restores captured index/worktree snapshots.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordRead`] when the journal is present but invalid, and apply
/// failures when rollback or checkout cannot complete.
/// Abort an incomplete rewrite apply by rolling refs back and restoring snapshots.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordRead`] when the journal is invalid and apply failures when
/// rollback cannot complete.
pub fn abort_interrupted_rewrite_apply(
    repo: &Repository,
) -> Result<RewriteApplyRecovery, HistoryError> {
    if !rewrite_apply_journal_active(repo.git_dir()) {
        return Err(HistoryError::RewriteApplyIncomplete);
    }
    let record = read_rewrite_apply_journal(repo)?;
    rollback_from_journal(repo, &record)
}

/// Try to finish or roll back a pending journal, then fail when it remains.
///
/// # Errors
///
/// Returns [`HistoryError::RewriteApplyIncomplete`] when recovery could not clear the journal.
pub fn ensure_rewrite_apply_ready(repo: &Repository) -> Result<(), HistoryError> {
    if !rewrite_apply_journal_active(repo.git_dir()) {
        return Ok(());
    }
    recover_interrupted_rewrite_apply(repo)?;
    if rewrite_apply_journal_active(repo.git_dir()) {
        return Err(HistoryError::RewriteApplyIncomplete);
    }
    Ok(())
}

/// Finish or roll back an interrupted rewrite apply recorded in the journal.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordRead`] when the journal is present but invalid, and apply
/// failures when rollback or checkout cannot complete.
pub fn recover_interrupted_rewrite_apply(
    repo: &Repository,
) -> Result<Option<RewriteApplyRecovery>, HistoryError> {
    if !rewrite_apply_journal_active(repo.git_dir()) {
        return Ok(None);
    }
    let record = read_rewrite_apply_journal(repo)?;
    let refs_at_new = refs_match_plan(repo, &record.ref_updates, true)?;
    let refs_at_old = refs_match_plan(repo, &record.ref_updates, false)?;

    if refs_at_new {
        let (Some(old_tree), Some(new_tree)) = (record.head_old_tree, record.head_new_tree) else {
            return rollback_from_journal(repo, &record).map(Some);
        };
        if let Err(checkout_err) = apply_head_tree_to_worktree(repo, Some(&old_tree), &new_tree) {
            let _ = rollback_from_journal(repo, &record);
            return Err(checkout_err);
        }
        clear_rewrite_apply_journal(repo)?;
        return Ok(Some(RewriteApplyRecovery::FinishedCheckout));
    }

    if refs_at_old {
        clear_rewrite_apply_journal(repo)?;
        return Ok(None);
    }

    rollback_from_journal(repo, &record).map(Some)
}

fn rollback_from_journal(
    repo: &Repository,
    record: &RewriteApplyJournal,
) -> Result<RewriteApplyRecovery, HistoryError> {
    rollback_ref_updates(repo, &record.ref_updates, &record.apply_options)?;
    restore_worktree_after_failed_apply(
        repo,
        record.index_before.as_deref(),
        record.worktree_before.as_ref(),
    )?;
    clear_rewrite_apply_journal(repo)?;
    Ok(RewriteApplyRecovery::RolledBack)
}

fn refs_match_plan(
    repo: &Repository,
    updates: &[PlannedRefUpdate],
    expect_new: bool,
) -> Result<bool, HistoryError> {
    for planned in updates {
        let current =
            resolve_ref(repo.git_dir(), &planned.name).map_err(|e| HistoryError::RefApply {
                message: e.to_string(),
            })?;
        let expected = if expect_new { planned.new } else { planned.old };
        if current != expected {
            return Ok(false);
        }
    }
    Ok(true)
}

struct RewriteApplyJournal {
    apply_options: ApplyOptions,
    head_old_tree: Option<ObjectId>,
    head_new_tree: Option<ObjectId>,
    ref_updates: Vec<PlannedRefUpdate>,
    index_before: Option<Vec<u8>>,
    worktree_before: Option<HashMap<String, WorktreePathSnapshot>>,
}

fn read_rewrite_apply_journal(repo: &Repository) -> Result<RewriteApplyJournal, HistoryError> {
    let dir = journal_dir(repo.git_dir());
    let meta = fs::read_to_string(dir.join(META)).map_err(journal_read_err)?;
    let mut parsed = parse_meta(&meta)?;
    parsed.index_before = Some(fs::read(dir.join(INDEX_BEFORE)).map_err(journal_read_err)?);
    parsed.worktree_before = Some(read_worktree_snapshot_dir(&dir.join(WORKTREE_DIR))?);
    Ok(parsed)
}

fn journal_dir(git_dir: &Path) -> PathBuf {
    git_dir.join("grit/rewrite-apply")
}

fn journal_write_err(e: impl std::fmt::Display) -> HistoryError {
    HistoryError::UndoRecordWrite {
        message: e.to_string(),
    }
}

fn journal_read_err(e: impl std::fmt::Display) -> HistoryError {
    HistoryError::UndoRecordRead {
        message: e.to_string(),
    }
}

fn serialize_meta(plan: &RewritePlan, opts: &ApplyOptions) -> String {
    let mut out = format!("version {FORMAT_VERSION}\n");
    out.push_str(&format!("apply-identity {}\n", opts.identity));
    out.push_str(&format!("apply-reflog {}\n", opts.reflog_message));
    if let Some(op) = &opts.record_operation {
        out.push_str(&format!("operation {op}\n"));
    }
    write_head_effect(&mut out, &plan.head);
    for u in &plan.ref_updates {
        out.push_str(&format!(
            "ref {} {} {}\n",
            u.name,
            u.old.to_hex(),
            u.new.to_hex()
        ));
    }
    out
}

fn write_head_effect(out: &mut String, head: &HeadEffect) {
    match &head.head_ref {
        Some(name) => out.push_str(&format!("head symbolic {name}\n")),
        None => out.push_str("head detached\n"),
    }
    if let (Some(old), Some(new)) = (head.old_tree, head.new_tree) {
        out.push_str(&format!("head-tree {} {}\n", old.to_hex(), new.to_hex()));
    }
}

fn parse_meta(text: &str) -> Result<RewriteApplyJournal, HistoryError> {
    let mut version: Option<u32> = None;
    let mut identity: Option<String> = None;
    let mut reflog: Option<String> = None;
    let mut record_operation: Option<String> = None;
    let mut head_old_tree: Option<ObjectId> = None;
    let mut head_new_tree: Option<ObjectId> = None;
    let mut ref_updates = Vec::new();

    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(4, ' ');
        let keyword = parts.next().unwrap_or_default();
        match keyword {
            "version" => {
                let raw = parts
                    .next()
                    .ok_or_else(|| parse_err(line_no, "missing version"))?;
                version = Some(
                    raw.parse::<u32>()
                        .map_err(|e| parse_err(line_no, &e.to_string()))?,
                );
            }
            "apply-identity" => {
                identity = Some(parts.collect::<Vec<_>>().join(" "));
            }
            "apply-reflog" => {
                reflog = Some(parts.collect::<Vec<_>>().join(" "));
            }
            "operation" => {
                record_operation = Some(parts.collect::<Vec<_>>().join(" "));
            }
            "head-tree" => {
                head_old_tree = Some(parse_oid(parts.next(), line_no, "head-tree old")?);
                head_new_tree = Some(parse_oid(parts.next(), line_no, "head-tree new")?);
            }
            "ref" => {
                let name = parts
                    .next()
                    .ok_or_else(|| parse_err(line_no, "missing ref name"))?
                    .to_owned();
                let old = parse_oid(parts.next(), line_no, "ref old")?;
                let new = parse_oid(parts.next(), line_no, "ref new")?;
                ref_updates.push(PlannedRefUpdate { name, old, new });
            }
            "head" => {
                let _mode = parts.next();
            }
            other => return Err(parse_err(line_no, &format!("unknown keyword {other}"))),
        }
    }

    if version != Some(FORMAT_VERSION) {
        return Err(HistoryError::UndoRecordRead {
            message: format!(
                "unsupported rewrite-apply journal version (expected {FORMAT_VERSION})"
            ),
        });
    }
    let apply_options = ApplyOptions {
        identity: identity.ok_or_else(|| parse_err(0, "missing apply-identity"))?,
        reflog_message: reflog.ok_or_else(|| parse_err(0, "missing apply-reflog"))?,
        record_operation,
        hooks: Default::default(),
    };
    Ok(RewriteApplyJournal {
        apply_options,
        head_old_tree,
        head_new_tree,
        ref_updates,
        index_before: None,
        worktree_before: None,
    })
}

fn parse_oid(token: Option<&str>, line_no: usize, label: &str) -> Result<ObjectId, HistoryError> {
    let hex = token.ok_or_else(|| parse_err(line_no, &format!("missing {label}")))?;
    ObjectId::from_hex(hex).map_err(|e| parse_err(line_no, &format!("{label}: {e}")))
}

fn parse_err(line_no: usize, detail: &str) -> HistoryError {
    HistoryError::UndoRecordRead {
        message: format!("rewrite-apply journal line {}: {detail}", line_no + 1),
    }
}

fn worktree_snapshot_file(dir: &Path, rel_path: &str) -> Result<PathBuf, HistoryError> {
    if rel_path.contains("..") {
        return Err(HistoryError::UndoRecordWrite {
            message: format!("invalid worktree snapshot path {rel_path:?}"),
        });
    }
    Ok(dir.join(rel_path))
}

fn write_worktree_snapshot_file(
    dir: &Path,
    path: &str,
    snap: &WorktreePathSnapshot,
) -> Result<(), HistoryError> {
    let file_path = worktree_snapshot_file(dir, path)?;
    if let Some(parent) = file_path.parent() {
        fs::create_dir_all(parent).map_err(journal_write_err)?;
    }
    let kind = match snap.kind {
        WorktreePathKind::Absent => "absent",
        WorktreePathKind::Directory => "directory",
        WorktreePathKind::Regular => "regular",
        WorktreePathKind::Symlink => "symlink",
    };
    let mut body = format!("path: {path}\nkind: {kind}\n");
    #[cfg(unix)]
    {
        body.push_str(&format!("mode: {:o}\n", snap.mode));
    }
    body.push_str("---\n");
    let mut file = fs::File::create(&file_path).map_err(journal_write_err)?;
    file.write_all(body.as_bytes()).map_err(journal_write_err)?;
    if let Some(bytes) = &snap.bytes {
        file.write_all(bytes).map_err(journal_write_err)?;
    }
    Ok(())
}

fn read_worktree_snapshot_dir(
    dir: &Path,
) -> Result<HashMap<String, WorktreePathSnapshot>, HistoryError> {
    let mut out = HashMap::new();
    if !dir.is_dir() {
        return Ok(out);
    }
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        for entry in fs::read_dir(&current).map_err(journal_read_err)? {
            let entry = entry.map_err(journal_read_err)?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            let rel = path
                .strip_prefix(dir)
                .map_err(journal_read_err)?
                .to_string_lossy()
                .replace('\\', "/");
            let bytes = fs::read(&path).map_err(journal_read_err)?;
            let (repo_path, snap) = parse_worktree_snapshot_file(&bytes)?;
            if repo_path != rel {
                return Err(HistoryError::UndoRecordRead {
                    message: format!(
                        "worktree snapshot path mismatch: header {repo_path} file {rel}"
                    ),
                });
            }
            out.insert(repo_path, snap);
        }
    }
    Ok(out)
}

fn parse_worktree_snapshot_file(
    bytes: &[u8],
) -> Result<(String, WorktreePathSnapshot), HistoryError> {
    let sep = b"\n---\n";
    let split_at = bytes
        .windows(sep.len())
        .position(|w| w == sep)
        .ok_or_else(|| HistoryError::UndoRecordRead {
            message: "worktree snapshot missing header terminator".into(),
        })?;
    let header =
        std::str::from_utf8(&bytes[..split_at]).map_err(|e| HistoryError::UndoRecordRead {
            message: e.to_string(),
        })?;
    let payload = bytes[(split_at + sep.len())..].to_vec();
    let mut repo_path: Option<String> = None;
    let mut kind: Option<WorktreePathKind> = None;
    #[cfg(unix)]
    let mut mode: u32 = 0o644;
    for line in header.lines() {
        let mut parts = line.splitn(2, ": ");
        match parts.next().unwrap_or_default() {
            "path" => repo_path = parts.next().map(str::to_owned),
            "kind" => {
                kind = Some(match parts.next().unwrap_or_default() {
                    "absent" => WorktreePathKind::Absent,
                    "directory" => WorktreePathKind::Directory,
                    "regular" => WorktreePathKind::Regular,
                    "symlink" => WorktreePathKind::Symlink,
                    other => {
                        return Err(HistoryError::UndoRecordRead {
                            message: format!("unknown worktree snapshot kind {other}"),
                        })
                    }
                });
            }
            "mode" => {
                #[cfg(unix)]
                {
                    if let Some(raw) = parts.next() {
                        mode = u32::from_str_radix(raw, 8).unwrap_or(0o644);
                    }
                }
            }
            _ => {}
        }
    }
    let path = repo_path.ok_or_else(|| HistoryError::UndoRecordRead {
        message: "worktree snapshot missing path".into(),
    })?;
    let kind = kind.ok_or_else(|| HistoryError::UndoRecordRead {
        message: format!("worktree snapshot for {path} missing kind"),
    })?;
    Ok((
        path,
        WorktreePathSnapshot {
            kind,
            bytes: if payload.is_empty() {
                None
            } else {
                Some(payload)
            },
            #[cfg(unix)]
            mode,
        },
    ))
}
