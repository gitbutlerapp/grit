//! Crash-safe journal for [`super::apply::apply_rewrite`] and related apply paths.
//!
//! When a rewrite updates refs before the worktree checkout finishes, a kill can leave refs at
//! the new tips while the index and worktree still reflect the old tree. A pending journal under
//! `.git/grit/rewrite-apply/` records inverse ref movements and pre-apply index/worktree
//! snapshots so recovery can finish or roll back the operation.

use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

use crate::lockfile::{LockFile, LockWait};
use crate::objects::ObjectId;
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

/// Which apply path created a pending journal (controls recovery).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RewriteApplyKind {
    /// Standard [`super::apply_rewrite`] checkout.
    Standard,
    /// [`super::apply_fixup_rewrite`]: recovery rolls back instead of a generic checkout.
    Fixup,
    /// [`super::apply_absorb_rewrite`]: recovery rolls back instead of a generic checkout.
    Absorb,
}

impl RewriteApplyKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Standard => "rewrite",
            Self::Fixup => "fixup",
            Self::Absorb => "absorb",
        }
    }

    fn parse(raw: &str) -> Result<Self, HistoryError> {
        match raw {
            "rewrite" => Ok(Self::Standard),
            "fixup" => Ok(Self::Fixup),
            "absorb" => Ok(Self::Absorb),
            other => Err(HistoryError::UndoRecordRead {
                message: format!("unknown rewrite-apply kind {other:?}"),
            }),
        }
    }
}

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
    apply_kind: RewriteApplyKind,
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

    let body = serialize_meta(plan, opts, apply_kind);
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
        if record.apply_kind != RewriteApplyKind::Standard {
            return rollback_from_journal(repo, &record).map(Some);
        }
        let (Some(old_tree), Some(new_tree)) = (record.head_old_tree, record.head_new_tree) else {
            return rollback_from_journal(repo, &record).map(Some);
        };
        if let Err(checkout_err) = apply_head_tree_to_worktree(
            repo,
            Some(&old_tree),
            &new_tree,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        ) {
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
        let current = repo
            .resolve_ref_oid(&planned.name)
            .map_err(|e| HistoryError::RefApply {
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
    apply_kind: RewriteApplyKind,
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

fn serialize_meta(plan: &RewritePlan, opts: &ApplyOptions, apply_kind: RewriteApplyKind) -> String {
    let mut out = format!("version {FORMAT_VERSION}\n");
    out.push_str(&format!("apply-kind {}\n", apply_kind.as_str()));
    out.push_str(&format!("apply-identity {}\n", opts.identity.header_line()));
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
    let mut apply_kind = RewriteApplyKind::Standard;
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
            "apply-kind" => {
                let raw = parts
                    .next()
                    .ok_or_else(|| parse_err(line_no, "missing apply kind"))?;
                apply_kind = RewriteApplyKind::parse(raw)?;
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
        identity: {
            let raw = identity.ok_or_else(|| parse_err(0, "missing apply-identity"))?;
            crate::ident::parse_signature(&raw)
                .ok_or_else(|| parse_err(0, "invalid apply-identity"))?
        },
        reflog_message: reflog.ok_or_else(|| parse_err(0, "missing apply-reflog"))?,
        record_operation,
        hooks: Default::default(),
    };
    Ok(RewriteApplyJournal {
        apply_options,
        apply_kind,
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

fn validate_repo_relative_path(path: &str) -> Result<(), HistoryError> {
    if path.is_empty() {
        return Err(HistoryError::UndoRecordWrite {
            message: "empty worktree snapshot path".into(),
        });
    }
    let parsed = Path::new(path);
    if parsed.is_absolute() {
        return Err(HistoryError::UndoRecordWrite {
            message: format!("absolute worktree snapshot path {path:?}"),
        });
    }
    for component in parsed.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(HistoryError::UndoRecordWrite {
                    message: format!("invalid worktree snapshot path {path:?}"),
                });
            }
        }
    }
    Ok(())
}

fn snapshot_blob_name(repo_path: &str) -> Result<String, HistoryError> {
    validate_repo_relative_path(repo_path)?;
    Ok(hex::encode(
        crate::objects::HashAlgo::Sha1.checksum_digest(repo_path.as_bytes()),
    ))
}

fn encode_snapshot_path(path: &str) -> Result<String, HistoryError> {
    validate_repo_relative_path(path)?;
    Ok(hex::encode(path.as_bytes()))
}

fn decode_snapshot_path_hex(hex_str: &str) -> Result<String, HistoryError> {
    let bytes = hex::decode(hex_str.trim()).map_err(|e| HistoryError::UndoRecordRead {
        message: format!("worktree snapshot path-hex decode: {e}"),
    })?;
    let path = String::from_utf8(bytes).map_err(|e| HistoryError::UndoRecordRead {
        message: format!("worktree snapshot path-hex utf-8: {e}"),
    })?;
    validate_repo_relative_path(&path).map_err(|e| match e {
        HistoryError::UndoRecordWrite { message } => HistoryError::UndoRecordRead { message },
        other => other,
    })?;
    Ok(path)
}

fn decode_snapshot_path_legacy(raw: &str) -> Result<String, HistoryError> {
    if raw.contains('\n') || raw.contains('\r') {
        return Err(HistoryError::UndoRecordRead {
            message: "legacy worktree snapshot path header must not contain newlines".into(),
        });
    }
    validate_repo_relative_path(raw).map_err(|e| match e {
        HistoryError::UndoRecordWrite { message } => HistoryError::UndoRecordRead { message },
        other => other,
    })?;
    Ok(raw.to_owned())
}

fn payload_kind_line(snap: &WorktreePathSnapshot) -> &'static str {
    match snap.kind {
        WorktreePathKind::Absent | WorktreePathKind::Directory => "payload: none",
        WorktreePathKind::Regular | WorktreePathKind::Symlink => match snap.bytes.as_ref() {
            None => "payload: none",
            Some(bytes) if bytes.is_empty() => "payload: empty",
            Some(_) => "payload: data",
        },
    }
}

fn write_worktree_snapshot_file(
    dir: &Path,
    path: &str,
    snap: &WorktreePathSnapshot,
) -> Result<(), HistoryError> {
    let file_path = dir.join(snapshot_blob_name(path)?);
    let kind = match snap.kind {
        WorktreePathKind::Absent => "absent",
        WorktreePathKind::Directory => "directory",
        WorktreePathKind::Regular => "regular",
        WorktreePathKind::Symlink => "symlink",
    };
    let mut body = format!("path-hex: {}\nkind: {kind}\n", encode_snapshot_path(path)?);
    body.push_str(payload_kind_line(snap));
    body.push('\n');
    #[cfg(unix)]
    {
        body.push_str(&format!("mode: {:o}\n", snap.mode));
    }
    body.push_str("---\n");
    let mut file = fs::File::create(&file_path).map_err(journal_write_err)?;
    file.write_all(body.as_bytes()).map_err(journal_write_err)?;
    if payload_kind_line(snap) == "payload: data" {
        if let Some(bytes) = &snap.bytes {
            file.write_all(bytes).map_err(journal_write_err)?;
        }
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
    for entry in fs::read_dir(dir).map_err(journal_read_err)? {
        let entry = entry.map_err(journal_read_err)?;
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let bytes = fs::read(&path).map_err(journal_read_err)?;
        let (repo_path, snap) = parse_worktree_snapshot_file(&bytes)?;
        out.insert(repo_path, snap);
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
    let mut repo_path_hex: Option<String> = None;
    let mut repo_path_legacy: Option<String> = None;
    let mut kind: Option<WorktreePathKind> = None;
    let mut payload_kind: Option<&str> = None;
    #[cfg(unix)]
    let mut mode: u32 = 0o644;
    for line in header.lines() {
        let mut parts = line.splitn(2, ": ");
        match parts.next().unwrap_or_default() {
            "path-hex" => repo_path_hex = parts.next().map(str::to_owned),
            "path" => repo_path_legacy = parts.next().map(str::to_owned),
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
            "payload" => {
                payload_kind = parts.next().map(str::trim);
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
    let path = if let Some(hex_str) = repo_path_hex {
        decode_snapshot_path_hex(&hex_str)?
    } else if let Some(legacy) = repo_path_legacy {
        decode_snapshot_path_legacy(&legacy)?
    } else {
        return Err(HistoryError::UndoRecordRead {
            message: "worktree snapshot missing path".into(),
        });
    };
    let kind = kind.ok_or_else(|| HistoryError::UndoRecordRead {
        message: format!("worktree snapshot for {path} missing kind"),
    })?;
    let bytes = match payload_kind {
        Some("empty") => Some(Vec::new()),
        Some("data") => Some(payload),
        Some("none") | None => None,
        Some(other) => {
            return Err(HistoryError::UndoRecordRead {
                message: format!("unknown payload kind {other:?} for {path}"),
            });
        }
    };
    Ok((
        path,
        WorktreePathSnapshot {
            kind,
            bytes,
            #[cfg(unix)]
            mode,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use crate::history::types::{ApplyOptions, HeadEffect, RewritePlan};
    use crate::repo::{init_repository, Repository};
    use crate::RefStorageFormat;

    fn test_apply_identity() -> crate::ident::Signature {
        crate::ident::parse_signature("t <t@t> 0 +0000")
            .unwrap_or_else(|| panic!("test apply identity must parse"))
    }

    fn temp_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        (dir, repo)
    }

    #[test]
    fn worktree_snapshot_roundtrip_git_path_with_double_dot_in_name() {
        let (_dir, repo) = temp_repo();
        let mut snaps = HashMap::new();
        snaps.insert(
            "a..b".into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(b"hello".to_vec()),
                #[cfg(unix)]
                mode: 0o644,
            },
        );
        let plan = RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        };
        let opts = ApplyOptions {
            identity: test_apply_identity(),
            reflog_message: "test".into(),
            record_operation: None,
            hooks: Default::default(),
        };
        write_rewrite_apply_journal(
            &repo,
            &plan,
            &opts,
            RewriteApplyKind::Standard,
            b"INDEX",
            &snaps,
        )
        .expect("write");
        let record = read_rewrite_apply_journal(&repo).expect("read");
        let restored = record.worktree_before.expect("snapshots");
        assert_eq!(
            restored.get("a..b").and_then(|s| s.bytes.as_deref()),
            Some(b"hello" as &[u8])
        );
        clear_rewrite_apply_journal(&repo).expect("clear");
    }

    #[test]
    fn worktree_snapshot_preserves_empty_regular_file_payload() {
        let (_dir, repo) = temp_repo();
        let mut snaps = HashMap::new();
        snaps.insert(
            "empty.txt".into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(Vec::new()),
                #[cfg(unix)]
                mode: 0o644,
            },
        );
        let plan = RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        };
        let opts = ApplyOptions {
            identity: test_apply_identity(),
            reflog_message: "test".into(),
            record_operation: None,
            hooks: Default::default(),
        };
        write_rewrite_apply_journal(
            &repo,
            &plan,
            &opts,
            RewriteApplyKind::Standard,
            b"INDEX",
            &snaps,
        )
        .expect("write");
        let record = read_rewrite_apply_journal(&repo).expect("read");
        let snap = record
            .worktree_before
            .expect("snapshots")
            .get("empty.txt")
            .cloned()
            .expect("empty.txt");
        assert_eq!(snap.bytes, Some(Vec::new()));
        clear_rewrite_apply_journal(&repo).expect("clear");
    }

    #[test]
    fn hash_keyed_snapshots_do_not_collide_for_nested_paths() {
        let (_dir, repo) = temp_repo();
        let mut snaps = HashMap::new();
        snaps.insert(
            "foo".into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(b"file".to_vec()),
                #[cfg(unix)]
                mode: 0o644,
            },
        );
        snaps.insert(
            "foo/bar".into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(b"nested".to_vec()),
                #[cfg(unix)]
                mode: 0o644,
            },
        );
        let plan = RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        };
        let opts = ApplyOptions {
            identity: test_apply_identity(),
            reflog_message: "test".into(),
            record_operation: None,
            hooks: Default::default(),
        };
        write_rewrite_apply_journal(
            &repo,
            &plan,
            &opts,
            RewriteApplyKind::Standard,
            b"INDEX",
            &snaps,
        )
        .expect("write");
        let record = read_rewrite_apply_journal(&repo).expect("read");
        let restored = record.worktree_before.expect("snapshots");
        assert_eq!(
            restored.get("foo").and_then(|s| s.bytes.as_deref()),
            Some(b"file" as &[u8])
        );
        assert_eq!(
            restored.get("foo/bar").and_then(|s| s.bytes.as_deref()),
            Some(b"nested" as &[u8])
        );
        clear_rewrite_apply_journal(&repo).expect("clear");
    }

    #[test]
    fn worktree_snapshot_roundtrip_git_path_with_newline_in_name() {
        let path = "line\nbreak";
        let (_dir, repo) = temp_repo();
        let mut snaps = HashMap::new();
        snaps.insert(
            path.into(),
            WorktreePathSnapshot {
                kind: WorktreePathKind::Regular,
                bytes: Some(b"payload".to_vec()),
                #[cfg(unix)]
                mode: 0o644,
            },
        );
        let plan = RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        };
        let opts = ApplyOptions {
            identity: test_apply_identity(),
            reflog_message: "test".into(),
            record_operation: None,
            hooks: Default::default(),
        };
        write_rewrite_apply_journal(
            &repo,
            &plan,
            &opts,
            RewriteApplyKind::Standard,
            b"INDEX",
            &snaps,
        )
        .expect("write");
        let record = read_rewrite_apply_journal(&repo).expect("read");
        let restored = record.worktree_before.expect("snapshots");
        assert_eq!(
            restored.get(path).and_then(|s| s.bytes.as_deref()),
            Some(b"payload" as &[u8])
        );
        clear_rewrite_apply_journal(&repo).expect("clear");
    }
}
