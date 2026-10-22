//! Persist the last applied history ref plan for [`super::undo::undo_last_rewrite`].

use std::io::Write;
use std::path::PathBuf;

use crate::lockfile::{LockFile, LockWait};
use crate::objects::ObjectId;
use crate::repo::Repository;

use super::error::HistoryError;
use super::types::{HeadEffect, PlannedRefUpdate, RewritePlan};

const FORMAT_VERSION: u32 = 1;

/// Parsed `.git/grit/history-last` contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoryLastRecord {
    /// Operation that produced this record (`drop`, `rebase`, `undo`, …).
    pub operation: String,
    /// Symbolic `HEAD` ref when checkout was on a branch; `None` when detached.
    pub head_ref: Option<String>,
    /// Commit at `HEAD` before the operation.
    pub head_old_commit: Option<ObjectId>,
    /// Commit at `HEAD` after the operation.
    pub head_new_commit: Option<ObjectId>,
    /// Tree at `HEAD` before the operation.
    pub head_old_tree: Option<ObjectId>,
    /// Tree at `HEAD` after the operation.
    pub head_new_tree: Option<ObjectId>,
    /// Ref tip movements applied by the operation.
    pub ref_updates: Vec<PlannedRefUpdate>,
}

impl HistoryLastRecord {
    /// Build a record from a rewrite plan that was just applied.
    #[must_use]
    pub fn from_applied_plan(operation: &str, plan: &RewritePlan) -> Self {
        Self {
            operation: operation.into(),
            head_ref: plan.head.head_ref.clone(),
            head_old_commit: plan.head.old_commit,
            head_new_commit: plan
                .head_change()
                .map(|(_, new)| new)
                .or(plan.head.old_commit),
            head_old_tree: plan.head.old_tree,
            head_new_tree: plan.head.new_tree,
            ref_updates: plan.ref_updates.clone(),
        }
    }

    /// Inverse ref updates suitable for undo.
    #[must_use]
    pub fn inverse_ref_updates(&self) -> Vec<PlannedRefUpdate> {
        self.ref_updates
            .iter()
            .map(|u| PlannedRefUpdate {
                name: u.name.clone(),
                old: u.new,
                new: u.old,
            })
            .collect()
    }

    /// [`RewritePlan`] describing an undo checkout (refs and `HEAD` trees swapped).
    #[must_use]
    pub fn undo_plan(&self) -> RewritePlan {
        RewritePlan {
            commits: Vec::new(),
            ref_updates: self.inverse_ref_updates(),
            head: HeadEffect {
                commit_changes: self.head_old_commit != self.head_new_commit,
                old_commit: self.head_new_commit.or(self.head_old_commit),
                head_ref: self.head_ref.clone(),
                old_tree: self.head_new_tree.or(self.head_old_tree),
                new_tree: self.head_old_tree,
            },
        }
    }
}

fn history_last_path(repo: &Repository) -> PathBuf {
    repo.git_dir().join("grit/history-last")
}

fn history_last_lock_path(repo: &Repository) -> PathBuf {
    repo.git_dir().join("grit/history-last.lock")
}

/// Write `record` to `.git/grit/history-last` atomically.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordWrite`] on I/O or lock failures.
pub fn write_history_last(
    repo: &Repository,
    record: &HistoryLastRecord,
) -> Result<(), HistoryError> {
    let grit_dir = repo.git_dir().join("grit");
    std::fs::create_dir_all(&grit_dir).map_err(|e| HistoryError::UndoRecordWrite {
        message: e.to_string(),
    })?;
    let body = serialize_record(record);
    let lock_path = history_last_lock_path(repo);
    let target = history_last_path(repo);
    let wait = LockWait::Immediately;
    let mut lock =
        LockFile::acquire(&lock_path, wait).map_err(|e| HistoryError::UndoRecordWrite {
            message: e.to_string(),
        })?;
    lock.write_all(body.as_bytes())
        .map_err(|e| HistoryError::UndoRecordWrite {
            message: e.to_string(),
        })?;
    lock.commit_to(&target)
        .map_err(|e| HistoryError::UndoRecordWrite {
            message: e.to_string(),
        })?;
    Ok(())
}

/// Read `.git/grit/history-last` when present.
///
/// # Errors
///
/// Returns [`HistoryError::UndoRecordRead`] when the file exists but is invalid.
pub fn read_history_last(repo: &Repository) -> Result<Option<HistoryLastRecord>, HistoryError> {
    let path = history_last_path(repo);
    if !path.is_file() {
        return Ok(None);
    }
    let bytes = std::fs::read(&path).map_err(|e| HistoryError::UndoRecordRead {
        message: e.to_string(),
    })?;
    parse_record(&bytes).map(Some)
}

fn serialize_record(record: &HistoryLastRecord) -> String {
    let mut out = format!("version {FORMAT_VERSION}\n");
    out.push_str(&format!("operation {}\n", record.operation));
    match &record.head_ref {
        Some(name) => out.push_str(&format!("head symbolic {name}\n")),
        None => out.push_str("head detached\n"),
    }
    if let (Some(old), Some(new)) = (record.head_old_commit, record.head_new_commit) {
        out.push_str(&format!("head-commit {} {}\n", old.to_hex(), new.to_hex()));
    }
    if let (Some(old), Some(new)) = (record.head_old_tree, record.head_new_tree) {
        out.push_str(&format!("head-tree {} {}\n", old.to_hex(), new.to_hex()));
    }
    for u in &record.ref_updates {
        out.push_str(&format!(
            "ref {} {} {}\n",
            u.name,
            u.old.to_hex(),
            u.new.to_hex()
        ));
    }
    out
}

fn parse_record(bytes: &[u8]) -> Result<HistoryLastRecord, HistoryError> {
    let text = std::str::from_utf8(bytes).map_err(|e| HistoryError::UndoRecordRead {
        message: e.to_string(),
    })?;
    let mut version: Option<u32> = None;
    let mut operation: Option<String> = None;
    let mut head_ref: Option<Option<String>> = None;
    let mut head_old_commit: Option<ObjectId> = None;
    let mut head_new_commit: Option<ObjectId> = None;
    let mut head_old_tree: Option<ObjectId> = None;
    let mut head_new_tree: Option<ObjectId> = None;
    let mut ref_updates = Vec::new();

    for (line_no, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let keyword = parts.next().unwrap_or_default();
        match keyword {
            "version" => {
                let v = parts
                    .next()
                    .ok_or_else(|| parse_err(line_no, "missing version number"))?
                    .parse::<u32>()
                    .map_err(|e| parse_err(line_no, &e.to_string()))?;
                version = Some(v);
            }
            "operation" => {
                let op = parts
                    .next()
                    .ok_or_else(|| parse_err(line_no, "missing operation name"))?;
                operation = Some(op.into());
            }
            "head" => {
                let mode = parts
                    .next()
                    .ok_or_else(|| parse_err(line_no, "missing head mode"))?;
                head_ref = Some(match mode {
                    "symbolic" => {
                        let name = parts
                            .next()
                            .ok_or_else(|| parse_err(line_no, "missing symbolic ref"))?;
                        Some(name.into())
                    }
                    "detached" => None,
                    other => return Err(parse_err(line_no, &format!("unknown head mode {other}"))),
                });
            }
            "head-commit" => {
                let old = parse_oid(parts.next(), line_no, "head-commit old")?;
                let new = parse_oid(parts.next(), line_no, "head-commit new")?;
                head_old_commit = Some(old);
                head_new_commit = Some(new);
            }
            "head-tree" => {
                let old = parse_oid(parts.next(), line_no, "head-tree old")?;
                let new = parse_oid(parts.next(), line_no, "head-tree new")?;
                head_old_tree = Some(old);
                head_new_tree = Some(new);
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
            other => return Err(parse_err(line_no, &format!("unknown keyword {other}"))),
        }
    }

    if version != Some(FORMAT_VERSION) {
        return Err(HistoryError::UndoRecordRead {
            message: format!("unsupported or missing version (expected {FORMAT_VERSION})"),
        });
    }
    let operation = operation.ok_or_else(|| HistoryError::UndoRecordRead {
        message: "missing operation line".into(),
    })?;
    let head_ref = match head_ref {
        Some(v) => v,
        None => {
            return Err(HistoryError::UndoRecordRead {
                message: "missing head line".into(),
            })
        }
    };

    Ok(HistoryLastRecord {
        operation,
        head_ref,
        head_old_commit,
        head_new_commit,
        head_old_tree,
        head_new_tree,
        ref_updates,
    })
}

fn parse_oid(token: Option<&str>, line_no: usize, label: &str) -> Result<ObjectId, HistoryError> {
    let hex = token.ok_or_else(|| parse_err(line_no, &format!("missing {label}")))?;
    ObjectId::from_hex(hex).map_err(|e| parse_err(line_no, &e.to_string()))
}

fn parse_err(line_no: usize, detail: &str) -> HistoryError {
    HistoryError::UndoRecordRead {
        message: format!("line {}: {detail}", line_no + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    #[test]
    fn record_roundtrip_text_format() {
        let record = HistoryLastRecord {
            operation: "drop".into(),
            head_ref: Some("refs/heads/main".into()),
            head_old_commit: Some(oid(1)),
            head_new_commit: Some(oid(2)),
            head_old_tree: Some(oid(3)),
            head_new_tree: Some(oid(4)),
            ref_updates: vec![PlannedRefUpdate {
                name: "refs/heads/main".into(),
                old: oid(1),
                new: oid(2),
            }],
        };
        let parsed = parse_record(serialize_record(&record).as_bytes()).expect("parse");
        assert_eq!(parsed, record);
    }
}
