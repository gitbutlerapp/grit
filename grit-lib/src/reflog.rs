//! Reflog reading and management.
//!
//! The reflog records updates to refs.  Each ref's log is stored at
//! `<git-dir>/logs/<refname>` (e.g. `logs/HEAD`, `logs/refs/heads/main`).
//! Each line has the format:
//!
//! ```text
//! <old-sha> <new-sha> <name> <<email>> <timestamp> <timezone>\t<message>
//! ```

use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::config::ConfigSet;
use crate::diff::zero_oid;
use crate::error::{Error, Result};
use crate::merge_base;
use crate::objects::{parse_commit, parse_tree, ObjectId, ObjectKind};
use crate::refs::{self, reflog_file_path};
use crate::repo::Repository;
use crate::wildmatch::{wildmatch, WM_PATHNAME};

/// A single reflog entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogEntry {
    /// Previous object ID.
    pub old_oid: ObjectId,
    /// New object ID.
    pub new_oid: ObjectId,
    /// Identity string: `"Name <email> timestamp tz"`.
    pub identity: String,
    /// The log message.
    pub message: String,
}

/// Return the filesystem path for a ref's reflog.
///
/// Uses the same storage rules as [`refs::append_reflog`] (branch reflogs under the
/// repository common directory for linked worktrees).
pub fn reflog_path(git_dir: &Path, refname: &str) -> PathBuf {
    reflog_file_path(git_dir, refname)
}

/// Remove the last line from a loose-ref reflog file, if present.
///
/// Used when rolling back a branch update after a failed `HEAD` reflog append.
///
/// # Errors
///
/// Returns I/O errors from reading or writing the reflog file.
pub fn truncate_last_reflog_line(git_dir: &Path, refname: &str) -> Result<()> {
    if crate::reftable::is_reftable_repo(git_dir) {
        return Ok(());
    }
    let path = reflog_path(git_dir, refname);
    let Ok(mut content) = fs::read_to_string(&path) else {
        return Ok(());
    };
    if content.is_empty() {
        return Ok(());
    }
    if content.ends_with('\n') {
        content.pop();
        if content.ends_with('\r') {
            content.pop();
        }
    }
    if let Some(pos) = content.rfind('\n') {
        content.truncate(pos + 1);
    } else {
        content.clear();
    }
    if content.is_empty() {
        let _ = fs::remove_file(&path);
    } else {
        fs::write(&path, content)?;
    }
    Ok(())
}

/// Apply `core.sharedRepository` permissions to a rewritten reflog file, matching Git's
/// `adjust_shared_perm` call in `files_reflog_expire`. Best-effort: ignores config and FS errors.
fn adjust_reflog_shared_perm(git_dir: &Path, path: &Path) {
    let Ok(config) = ConfigSet::load(
        &crate::environment::Environment::empty(),
        Some(git_dir),
        true,
    ) else {
        return;
    };
    let raw = config.get("core.sharedRepository");
    let Ok(perm) = crate::shared_repo::shared_repository_from_config_value(raw.as_deref()) else {
        return;
    };
    if perm != 0 {
        let _ = crate::shared_repo::adjust_shared_perm_path(perm, path);
    }
}

/// Check whether a reflog exists for the given ref.
pub fn reflog_exists(git_dir: &Path, refname: &str) -> bool {
    if crate::reftable::is_reftable_repo(git_dir) {
        return crate::reftable::reftable_reflog_exists(git_dir, refname);
    }
    let path = reflog_path(git_dir, refname);
    path.is_file()
}

/// Read a reflog using Git's loose ref DWIM rules when the direct path is missing.
///
/// Tries `refname`, then `refs/<refname>`, then `refs/heads/<refname>` (when `refname` is not
/// already under `refs/`). Matches `read_complete_reflog` in Git's `reflog-walk.c`.
pub fn read_reflog_dwim(git_dir: &Path, refname: &str) -> Result<Vec<ReflogEntry>> {
    let mut entries = read_reflog(git_dir, refname)?;
    if !entries.is_empty() {
        return Ok(entries);
    }
    if !refname.starts_with("refs/") {
        entries = read_reflog(git_dir, &format!("refs/{refname}"))?;
        if !entries.is_empty() {
            return Ok(entries);
        }
        entries = read_reflog(git_dir, &format!("refs/heads/{refname}"))?;
    }
    Ok(entries)
}

/// Read all reflog entries for the given ref, in file order (oldest first).
///
/// Returns an empty vec if the reflog file does not exist.
pub fn read_reflog(git_dir: &Path, refname: &str) -> Result<Vec<ReflogEntry>> {
    if crate::reftable::is_reftable_repo(git_dir) {
        return crate::reftable::reftable_read_reflog(git_dir, refname);
    }
    let path = reflog_path(git_dir, refname);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::Io(e)),
    };

    let mut entries = Vec::new();
    for line in content.lines() {
        if line.is_empty() {
            continue;
        }
        if let Some(entry) = parse_reflog_line(line) {
            entries.push(entry);
        }
    }
    Ok(entries)
}

/// Parse a single reflog line.
///
/// Format: `<old-hex> <new-hex> <identity>\t<message>`
fn parse_reflog_line(line: &str) -> Option<ReflogEntry> {
    // Split on tab first to separate identity from message
    let (before_tab, message) = if let Some(pos) = line.find('\t') {
        (&line[..pos], line[pos + 1..].to_string())
    } else {
        (line, String::new())
    };

    // Format: `<old-hex> <new-hex> <identity>`. The two OIDs are whitespace-
    // delimited; their hex width (40 for SHA-1, 64 for SHA-256) is inferred by
    // the parser, so split on the first two spaces rather than fixed offsets.
    let mut parts = before_tab.splitn(3, ' ');
    let old_hex = parts.next()?;
    let new_hex = parts.next()?;
    let identity = parts.next()?.to_string();

    let old_oid = old_hex.parse::<ObjectId>().ok()?;
    let new_oid = new_hex.parse::<ObjectId>().ok()?;

    Some(ReflogEntry {
        old_oid,
        new_oid,
        identity,
        message,
    })
}

/// Collect every non-null object ID mentioned in any file under `logs/` (recursive).
///
/// Used by `fsck` to validate reflog entries. Skips reftable-backed repos (no file logs).
pub fn all_reflog_oids(git_dir: &Path) -> Result<HashSet<ObjectId>> {
    if crate::reftable::is_reftable_repo(git_dir) {
        return Ok(HashSet::new());
    }
    let mut out = HashSet::new();
    let logs = git_dir.join("logs");
    if !logs.is_dir() {
        return Ok(out);
    }
    let z = zero_oid();
    walk_reflog_files(&logs, &mut out, &z)?;
    Ok(out)
}

/// Like [`all_reflog_oids`], but returns the OIDs in Git's `add_reflogs_to_pending`
/// insertion order so an equal-date priority-queue walk breaks ties the same way Git does.
///
/// Git iterates every reflog in sorted ref-name order (HEAD first, then `refs/...`), reads each
/// reflog file oldest-first, and inserts `old_oid` then `new_oid` per entry. The first occurrence
/// of each OID wins (later duplicates are dropped). `--reflog` on `git log`/`git rev-list` relies
/// on this order so that commits sharing a committer timestamp are emitted in a stable,
/// Git-compatible sequence.
pub fn all_reflog_oids_ordered(git_dir: &Path) -> Result<Vec<ObjectId>> {
    if crate::reftable::is_reftable_repo(git_dir) {
        // Reftable repos: fall back to the ref-name-ordered reflog walk for a deterministic order.
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let z = zero_oid();
        let mut names = list_reflog_refs(git_dir).unwrap_or_default();
        names.sort();
        for refname in names {
            for entry in read_reflog(git_dir, &refname).unwrap_or_default() {
                for oid in [entry.old_oid, entry.new_oid] {
                    if oid != z && seen.insert(oid) {
                        out.push(oid);
                    }
                }
            }
        }
        return Ok(out);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let logs = git_dir.join("logs");
    if !logs.is_dir() {
        return Ok(out);
    }
    let z = zero_oid();
    let mut names = list_reflog_refs(git_dir).unwrap_or_default();
    names.sort();
    for refname in names {
        let path = reflog_path(git_dir, &refname);
        let Ok(content) = fs::read_to_string(&path) else {
            continue;
        };
        for line in content.lines() {
            let Some(e) = parse_reflog_line(line) else {
                continue;
            };
            for oid in [e.old_oid, e.new_oid] {
                if oid != z && seen.insert(oid) {
                    out.push(oid);
                }
            }
        }
    }
    Ok(out)
}

fn walk_reflog_files(dir: &Path, out: &mut HashSet<ObjectId>, zero: &ObjectId) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        if path.is_dir() {
            walk_reflog_files(&path, out, zero)?;
        } else if path.is_file() {
            let content = fs::read_to_string(&path).map_err(Error::Io)?;
            for line in content.lines() {
                if let Some(e) = parse_reflog_line(line) {
                    if e.old_oid != *zero {
                        out.insert(e.old_oid);
                    }
                    if e.new_oid != *zero {
                        out.insert(e.new_oid);
                    }
                }
            }
        }
    }
    Ok(())
}

/// Delete specific reflog entries by index (0-based, newest-first order).
///
/// Rewrites the reflog file, omitting entries at the given indices.
pub fn delete_reflog_entries(git_dir: &Path, refname: &str, indices: &[usize]) -> Result<()> {
    let mut entries = read_reflog(git_dir, refname)?;
    if entries.is_empty() {
        return Ok(());
    }

    // Indices are in newest-first order (like show), so reverse the entries
    // to map indices correctly.
    entries.reverse();

    let indices_set: std::collections::HashSet<usize> = indices.iter().copied().collect();

    let remaining: Vec<&ReflogEntry> = entries
        .iter()
        .enumerate()
        .filter(|(i, _)| !indices_set.contains(i))
        .map(|(_, e)| e)
        .collect();

    // Write back in file order (oldest first), so reverse again
    let mut lines = Vec::new();
    for entry in remaining.iter().rev() {
        lines.push(format_reflog_entry(entry));
    }

    if crate::reftable::is_reftable_repo(git_dir) {
        let kept: Vec<ReflogEntry> = remaining
            .iter()
            .rev()
            .map(|entry| (*entry).clone())
            .collect();
        return crate::reftable::reftable_replace_reflog(git_dir, refname, &kept);
    }

    let path = reflog_path(git_dir, refname);
    fs::write(&path, lines.join(""))?;
    Ok(())
}

/// Expire (prune) reflog entries older than a given timestamp (Unix seconds).
///
/// If `expire_time` is `None`, removes all entries.
pub fn expire_reflog(git_dir: &Path, refname: &str, expire_time: Option<i64>) -> Result<usize> {
    let entries = read_reflog(git_dir, refname)?;
    if entries.is_empty() {
        return Ok(0);
    }

    let mut kept = Vec::new();
    let mut kept_entries = Vec::new();
    let mut pruned = 0usize;

    for entry in &entries {
        let ts = parse_timestamp_from_identity(&entry.identity);
        let dominated = match (expire_time, ts) {
            (Some(cutoff), Some(t)) => t < cutoff,
            (None, _) => true,        // expire all
            (Some(_), None) => false, // can't parse => keep
        };
        if dominated {
            pruned += 1;
        } else {
            kept_entries.push(entry.clone());
            kept.push(format_reflog_entry(entry));
        }
    }

    if crate::reftable::is_reftable_repo(git_dir) {
        crate::reftable::reftable_replace_reflog(git_dir, refname, &kept_entries)?;
        return Ok(pruned);
    }
    let path = reflog_path(git_dir, refname);
    fs::write(&path, kept.join(""))?;
    Ok(pruned)
}

/// Expire reflog entries whose `new_oid` is not an ancestor of the current ref tip
/// and whose identity timestamp is older than `cutoff` (Unix seconds).
///
/// Entries with an all-zero `new_oid` are never removed by this pass.
///
/// When `cutoff` is `None`, no entries are removed.
///
/// Reftable-backed repositories are skipped until reflog rewrite is implemented there.
pub fn expire_reflog_unreachable(
    repo: &Repository,
    git_dir: &Path,
    refname: &str,
    cutoff: Option<i64>,
) -> Result<usize> {
    let Some(cutoff) = cutoff else {
        return Ok(0);
    };
    if crate::reftable::is_reftable_repo(git_dir) {
        return Ok(0);
    }
    let tip = match refs::resolve_ref(git_dir, refname) {
        Ok(o) => o,
        Err(_) => return Ok(0),
    };
    let ancestors = match merge_base::ancestor_closure(repo, tip) {
        Ok(a) => a,
        Err(_) => return Ok(0),
    };

    let entries = read_reflog(git_dir, refname)?;
    if entries.is_empty() {
        return Ok(0);
    }

    let path = reflog_path(git_dir, refname);
    let mut kept = Vec::new();
    let mut pruned = 0usize;

    for entry in &entries {
        let ts = parse_timestamp_from_identity(&entry.identity);
        let unreachable = !entry.new_oid.is_zero() && !ancestors.contains(&entry.new_oid);
        let should_prune = unreachable && matches!(ts, Some(t) if t < cutoff);
        if should_prune {
            pruned += 1;
        } else {
            kept.push(format_reflog_entry(entry));
        }
    }

    fs::write(&path, kept.join(""))?;
    Ok(pruned)
}

/// Format a reflog entry back into the on-disk line format.
fn format_reflog_entry(entry: &ReflogEntry) -> String {
    if entry.message.is_empty() {
        format!("{} {} {}\n", entry.old_oid, entry.new_oid, entry.identity)
    } else {
        format!(
            "{} {} {}\t{}\n",
            entry.old_oid, entry.new_oid, entry.identity, entry.message
        )
    }
}

/// Extract the Unix timestamp from an identity string.
///
/// Identity format: `Name <email> <timestamp> <tz>`
fn parse_timestamp_from_identity(identity: &str) -> Option<i64> {
    // Walk backwards: last token is tz (+0000), second-to-last is timestamp
    let parts: Vec<&str> = identity.rsplitn(3, ' ').collect();
    if parts.len() >= 2 {
        parts[1].parse::<i64>().ok()
    } else {
        None
    }
}

/// Copy `logs/<branch_refname>` to `logs/HEAD` when keeping symbolic-HEAD reflogs aligned with
/// the checked-out branch (matches Git).
pub fn mirror_branch_reflog_to_head(git_dir: &Path, branch_refname: &str) -> Result<()> {
    if crate::reftable::is_reftable_repo(git_dir) {
        return Ok(());
    }
    let src = reflog_path(git_dir, branch_refname);
    if !src.is_file() {
        return Ok(());
    }
    let content = fs::read_to_string(&src).map_err(Error::Io)?;
    let dst = reflog_path(git_dir, "HEAD");
    if let Some(parent) = dst.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    fs::write(&dst, content).map_err(Error::Io)?;
    Ok(())
}

/// List all refs that have reflogs.
pub fn list_reflog_refs(git_dir: &Path) -> Result<Vec<String>> {
    if crate::reftable::is_reftable_repo(git_dir) {
        return crate::reftable::reftable_list_reflog_refs(git_dir);
    }
    let mut refs = Vec::new();
    let mut seen = HashSet::new();

    fn collect_from_logs_root(
        logs_dir: &Path,
        out: &mut Vec<String>,
        seen: &mut HashSet<String>,
        skip_per_worktree_refs: bool,
    ) -> Result<()> {
        if logs_dir.join("HEAD").is_file() && seen.insert("HEAD".to_string()) {
            out.push("HEAD".to_string());
        }
        let refs_logs = logs_dir.join("refs");
        if refs_logs.is_dir() {
            collect_reflog_refs(&refs_logs, "refs", out, seen, skip_per_worktree_refs)?;
        }
        Ok(())
    }

    collect_from_logs_root(&git_dir.join("logs"), &mut refs, &mut seen, false)?;
    if let Some(common) = refs::common_dir(git_dir) {
        if common != git_dir {
            collect_from_logs_root(&common.join("logs"), &mut refs, &mut seen, true)?;
        }
    }

    Ok(refs)
}

fn collect_reflog_refs(
    dir: &Path,
    prefix: &str,
    out: &mut Vec<String>,
    seen: &mut HashSet<String>,
    skip_per_worktree_refs: bool,
) -> Result<()> {
    let read_dir = match fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(Error::Io(e)),
    };

    for entry in read_dir {
        let entry = entry.map_err(Error::Io)?;
        let name = entry.file_name().to_string_lossy().to_string();
        let full_name = format!("{prefix}/{name}");
        if skip_per_worktree_refs && crate::worktree_ref::is_per_worktree_ref(&full_name) {
            continue;
        }
        let ft = entry.file_type().map_err(Error::Io)?;
        if ft.is_dir() {
            collect_reflog_refs(&entry.path(), &full_name, out, seen, skip_per_worktree_refs)?;
        } else if ft.is_file() && seen.insert(full_name.clone()) {
            out.push(full_name);
        }
    }
    Ok(())
}

// --- `git reflog expire` -----------------------------------------------------

/// Options for [`expire_reflog_git`].
#[derive(Debug, Clone)]
pub struct ReflogExpireParams {
    /// Prune entries whose commits fail a completeness walk (missing objects).
    pub stale_fix: bool,
    pub dry_run: bool,
    pub verbose: bool,
}

/// What happened to one reflog entry during [`expire_reflog_git`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReflogExpireActionKind {
    /// Entry was kept in the reflog.
    Keep,
    /// Entry was removed from the reflog.
    Prune,
    /// Entry would be removed when not in dry-run mode.
    WouldPrune,
}

/// Per-entry report from [`expire_reflog_git`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogExpireAction {
    /// Ref whose reflog was processed.
    pub refname: String,
    /// The reflog entry that was considered.
    pub entry: ReflogEntry,
    /// Keep vs prune (or dry-run would-prune).
    pub action: ReflogExpireActionKind,
}

/// Result of [`expire_reflog_git`]: prune count plus a per-entry action list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReflogExpireResult {
    /// Number of entries pruned (or that would be pruned in dry-run mode).
    pub pruned: usize,
    /// One action per reflog entry that was scanned.
    pub actions: Vec<ReflogExpireAction>,
}

/// Per-ref `gc.<pattern>.reflogExpire*` rule from config.
#[derive(Debug, Clone)]
pub struct GcReflogPattern {
    pattern: String,
    expire_total: Option<i64>,
    expire_unreachable: Option<i64>,
}

fn collect_gc_reflog_patterns(config: &ConfigSet, now: i64) -> Vec<GcReflogPattern> {
    let mut by_pattern: HashMap<String, GcReflogPattern> = HashMap::new();
    for e in config.entries() {
        let key = e.key.as_str();
        let Some(rest) = key.strip_prefix("gc.") else {
            continue;
        };
        // Per-ref: `gc.<wildmatch-pattern>.reflogExpire` (pattern may contain dots).
        // Global `gc.reflogExpire` has no pattern segment — see [`global_gc_reflog_expiry`].
        let lower = rest.to_ascii_lowercase();
        let (pat, is_total) = if lower.ends_with(".reflogexpireunreachable") {
            (
                &rest[..rest.len() - ".reflogexpireunreachable".len()],
                false,
            )
        } else if lower.ends_with(".reflogexpire") {
            (&rest[..rest.len() - ".reflogexpire".len()], true)
        } else {
            continue;
        };
        if pat.is_empty() {
            continue;
        }
        let Some(val) = e.value.as_deref() else {
            continue;
        };
        let Ok(ts) = parse_gc_reflog_expiry(val, now) else {
            continue;
        };
        let ent = by_pattern
            .entry(pat.to_string())
            .or_insert(GcReflogPattern {
                pattern: pat.to_string(),
                expire_total: None,
                expire_unreachable: None,
            });
        if is_total {
            ent.expire_total = Some(ts);
        } else {
            ent.expire_unreachable = Some(ts);
        }
    }
    by_pattern.into_values().collect()
}

fn global_gc_reflog_expiry(config: &ConfigSet, now: i64) -> (Option<i64>, Option<i64>) {
    let total = config
        .get("gc.reflogExpire")
        .and_then(|v| parse_gc_reflog_expiry(&v, now).ok());
    let unreach = config
        .get("gc.reflogExpireUnreachable")
        .and_then(|v| parse_gc_reflog_expiry(&v, now).ok());
    (total, unreach)
}

/// Parse `gc.reflogExpire` values: `never` / `false` → keep forever (`0`), else days or epoch.
fn parse_gc_reflog_expiry(raw: &str, now: i64) -> Result<i64> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("never") || s.eq_ignore_ascii_case("false") {
        return Ok(0);
    }
    if s.eq_ignore_ascii_case("now") || s.eq_ignore_ascii_case("all") {
        return Ok(i64::MAX);
    }
    let mut err = 0;
    let ts = crate::git_date::approx::approxidate_careful_at(s, now, Some(&mut err));
    if err != 0 {
        return Err(Error::Message(format!("invalid reflog expiry: {raw:?}")));
    }
    i64::try_from(ts).map_err(|_| Error::Message(format!("reflog expiry out of range: {raw:?}")))
}

/// Default `gc.reflogExpire`: reachable reflog entries older than this are dropped.
const REFLOG_DEFAULT_EXPIRE_REACHABLE_DAYS: i64 = 90;
/// Default `gc.reflogExpireUnreachable`: unreachable entries older than this are dropped.
const REFLOG_DEFAULT_EXPIRE_UNREACHABLE_DAYS: i64 = 30;

fn default_expire_cutoff(now: i64, days: i64) -> i64 {
    now.saturating_sub(time::Duration::days(days).whole_seconds())
}

fn default_expire_total(now: i64) -> i64 {
    default_expire_cutoff(now, REFLOG_DEFAULT_EXPIRE_REACHABLE_DAYS)
}

fn default_expire_unreachable(now: i64) -> i64 {
    default_expire_cutoff(now, REFLOG_DEFAULT_EXPIRE_UNREACHABLE_DAYS)
}

fn resolve_expire_for_ref(
    refname: &str,
    explicit_total: Option<i64>,
    explicit_unreachable: Option<i64>,
    patterns: &[GcReflogPattern],
    default_total: i64,
    default_unreachable: i64,
) -> (i64, i64) {
    let mut expire_total = explicit_total.unwrap_or(default_total);
    let mut expire_unreachable = explicit_unreachable.unwrap_or(default_unreachable);
    if explicit_total.is_some() && explicit_unreachable.is_some() {
        return (expire_total, expire_unreachable);
    }
    for ent in patterns {
        let wildcard_prefix_matches = ent
            .pattern
            .split_once('*')
            .is_some_and(|(prefix, _)| refname.starts_with(prefix));
        if wildmatch(ent.pattern.as_bytes(), refname.as_bytes(), WM_PATHNAME)
            || wildmatch(ent.pattern.as_bytes(), refname.as_bytes(), 0)
            || wildcard_prefix_matches
        {
            // Partial per-pattern config only sets one key; the other keeps the global/default.
            if explicit_total.is_none() {
                if let Some(total) = ent.expire_total {
                    expire_total = total;
                }
            }
            if explicit_unreachable.is_none() {
                if let Some(unreachable) = ent.expire_unreachable {
                    expire_unreachable = unreachable;
                }
            }
            return (expire_total, expire_unreachable);
        }
    }
    if refname == "refs/stash" {
        if explicit_total.is_none() {
            expire_total = 0;
        }
        if explicit_unreachable.is_none() {
            expire_unreachable = 0;
        }
    }
    (expire_total, expire_unreachable)
}

fn tree_fully_complete(repo: &Repository, oid: ObjectId, depth: usize) -> bool {
    if depth > 65536 {
        return false;
    }
    let Ok(obj) = repo.odb.read(&oid) else {
        return false;
    };
    match obj.kind {
        ObjectKind::Blob => true,
        ObjectKind::Tree => {
            let Ok(entries) = parse_tree(&obj.data) else {
                return false;
            };
            for e in entries {
                if !tree_fully_complete(repo, e.oid, depth + 1) {
                    return false;
                }
            }
            true
        }
        _ => false,
    }
}

fn commit_chain_complete(repo: &Repository, oid: ObjectId, depth: usize) -> bool {
    if oid.is_zero() {
        return true;
    }
    if depth > 65536 {
        return false;
    }
    let Ok(obj) = repo.odb.read(&oid) else {
        return false;
    };
    if obj.kind != ObjectKind::Commit {
        return false;
    }
    let Ok(c) = parse_commit(&obj.data) else {
        return false;
    };
    if !tree_fully_complete(repo, c.tree, depth + 1) {
        return false;
    }
    for p in &c.parents {
        if !commit_chain_complete(repo, *p, depth + 1) {
            return false;
        }
    }
    true
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UnreachableKind {
    Always,
    Normal,
    Head,
}

fn is_head_ref(refname: &str) -> bool {
    refname == "HEAD" || refname.ends_with("/HEAD")
}

fn tip_commits_for_reflog(repo: &Repository, git_dir: &Path, refname: &str) -> Vec<ObjectId> {
    let mut tips = Vec::new();
    if is_head_ref(refname) {
        if let Ok(oid) = refs::resolve_ref(git_dir, "HEAD") {
            tips.push(oid);
        }
        if let Ok(refs) = refs::list_refs(git_dir, "refs/") {
            for (_, oid) in refs {
                tips.push(oid);
            }
        }
    } else if let Ok(oid) = refs::resolve_ref(git_dir, refname) {
        tips.push(oid);
    }
    tips.sort();
    tips.dedup();
    tips.retain(|o| commit_chain_complete(repo, *o, 0));
    tips
}

fn reachable_commit_set(repo: &Repository, tips: &[ObjectId]) -> HashSet<ObjectId> {
    let mut acc = HashSet::new();
    for t in tips {
        if let Ok(cl) = merge_base::ancestor_closure(repo, *t) {
            acc.extend(cl);
        }
    }
    acc
}

fn is_unreachable_oid(
    repo: &Repository,
    reachable: &HashSet<ObjectId>,
    kind: UnreachableKind,
    oid: ObjectId,
) -> bool {
    if oid.is_zero() {
        return false;
    }
    if reachable.contains(&oid) {
        return false;
    }
    if kind == UnreachableKind::Always {
        return true;
    }
    let Ok(obj) = repo.odb.read(&oid) else {
        return true;
    };
    obj.kind == ObjectKind::Commit
}

fn should_drop_reflog_entry(
    repo: &Repository,
    entry: &ReflogEntry,
    expire_total: i64,
    expire_unreachable: i64,
    unreachable_kind: UnreachableKind,
    reachable: &HashSet<ObjectId>,
    stale_fix: bool,
) -> bool {
    let ts = parse_timestamp_from_identity(&entry.identity).unwrap_or(i64::MAX);
    if expire_total > 0 && ts < expire_total {
        return true;
    }
    if stale_fix
        && (!commit_chain_complete(repo, entry.old_oid, 0)
            || !commit_chain_complete(repo, entry.new_oid, 0))
    {
        return true;
    }
    if expire_unreachable > 0 && ts < expire_unreachable {
        match unreachable_kind {
            UnreachableKind::Always => return true,
            UnreachableKind::Normal | UnreachableKind::Head => {
                if is_unreachable_oid(repo, reachable, unreachable_kind, entry.old_oid)
                    || is_unreachable_oid(repo, reachable, unreachable_kind, entry.new_oid)
                {
                    return true;
                }
            }
        }
    }
    false
}

/// Git-compatible reflog expiry for one ref.
#[expect(clippy::too_many_arguments)]
pub fn expire_reflog_git(
    repo: &Repository,
    git_dir: &Path,
    refname: &str,
    params: &ReflogExpireParams,
    explicit_total: Option<i64>,
    explicit_unreachable: Option<i64>,
    gc_patterns: &[GcReflogPattern],
    gc_global_total: Option<i64>,
    gc_global_unreachable: Option<i64>,
    now: i64,
) -> Result<ReflogExpireResult> {
    let is_reftable = crate::reftable::is_reftable_repo(git_dir);
    let base_total = gc_global_total.unwrap_or_else(|| default_expire_total(now));
    let base_unreachable = gc_global_unreachable.unwrap_or_else(|| default_expire_unreachable(now));
    let (expire_total, expire_unreachable) = resolve_expire_for_ref(
        refname,
        explicit_total,
        explicit_unreachable,
        gc_patterns,
        base_total,
        base_unreachable,
    );

    let unreachable_kind = if expire_unreachable <= expire_total {
        UnreachableKind::Always
    } else if expire_unreachable == 0 || is_head_ref(refname) {
        UnreachableKind::Head
    } else {
        match refs::resolve_ref(git_dir, refname) {
            Ok(t) if commit_chain_complete(repo, t, 0) => UnreachableKind::Normal,
            _ => UnreachableKind::Always,
        }
    };

    let tips = tip_commits_for_reflog(repo, git_dir, refname);
    let reachable = if matches!(unreachable_kind, UnreachableKind::Always) {
        HashSet::new()
    } else {
        reachable_commit_set(repo, &tips)
    };

    let entries = read_reflog(git_dir, refname)?;
    if entries.is_empty() {
        return Ok(ReflogExpireResult {
            pruned: 0,
            actions: Vec::new(),
        });
    }
    let mut kept = Vec::new();
    let mut kept_entries = Vec::new();
    let mut pruned = 0usize;
    let mut actions = Vec::with_capacity(entries.len());

    for entry in &entries {
        let drop = should_drop_reflog_entry(
            repo,
            entry,
            expire_total,
            expire_unreachable,
            unreachable_kind,
            &reachable,
            params.stale_fix,
        );
        let action = if drop {
            pruned += 1;
            if params.dry_run {
                ReflogExpireActionKind::WouldPrune
            } else {
                ReflogExpireActionKind::Prune
            }
        } else {
            kept_entries.push(entry.clone());
            kept.push(format_reflog_entry(entry));
            ReflogExpireActionKind::Keep
        };
        actions.push(ReflogExpireAction {
            refname: refname.to_string(),
            entry: entry.clone(),
            action,
        });
    }

    if !params.dry_run && pruned > 0 {
        if is_reftable {
            crate::reftable::reftable_replace_reflog(git_dir, refname, &kept_entries)?;
        } else {
            // Git rewrites the reflog in place via the lockfile machinery and keeps the file even
            // when all entries are pruned (it does not unlink it — only an explicit reflog/ref
            // delete removes the file). Writing an empty file mirrors that and preserves the
            // file's existence. Git then runs `adjust_shared_perm` on the rewritten log, so honor
            // `core.sharedRepository` here too (t0600 "reflog expire honors core.sharedRepository").
            let path = reflog_path(git_dir, refname);
            fs::write(&path, kept.join(""))?;
            adjust_reflog_shared_perm(git_dir, &path);
        }
    }
    Ok(ReflogExpireResult { pruned, actions })
}

/// Per-ref `gc.<pattern>.reflogExpire*` rules plus global `gc.reflogExpire` / `gc.reflogExpireUnreachable`.
#[derive(Debug, Clone)]
pub struct GcReflogExpireConfig {
    pub patterns: Vec<GcReflogPattern>,
    pub global_total: Option<i64>,
    pub global_unreachable: Option<i64>,
}

/// Load gc reflog expiry rules from merged config (same layering as Git `reflog_expire_config`).
#[must_use]
pub fn load_gc_reflog_expire_config(config: &ConfigSet, now: i64) -> GcReflogExpireConfig {
    let (global_total, global_unreachable) = global_gc_reflog_expiry(config, now);
    GcReflogExpireConfig {
        patterns: collect_gc_reflog_patterns(config, now),
        global_total,
        global_unreachable,
    }
}

/// Best-effort object set for `--stale-fix` (refs + reflog mentions).
pub fn mark_stalefix_reachable(repo: &Repository, git_dir: &Path) -> Result<HashSet<ObjectId>> {
    let mut seeds: Vec<ObjectId> = Vec::new();
    if let Ok(oid) = refs::resolve_ref(git_dir, "HEAD") {
        seeds.push(oid);
    }
    if let Ok(refs) = refs::list_refs(git_dir, "refs/") {
        for (_, oid) in refs {
            seeds.push(oid);
        }
    }
    if let Ok(names) = list_reflog_refs(git_dir) {
        for r in names {
            if let Ok(ent) = read_reflog(git_dir, &r) {
                for e in ent {
                    if !e.old_oid.is_zero() {
                        seeds.push(e.old_oid);
                    }
                    if !e.new_oid.is_zero() {
                        seeds.push(e.new_oid);
                    }
                }
            }
        }
    }
    seeds.sort();
    seeds.dedup();

    let mut seen = HashSet::new();
    let mut queue: std::collections::VecDeque<ObjectId> = seeds.into_iter().collect();
    while let Some(oid) = queue.pop_front() {
        if oid.is_zero() || !seen.insert(oid) {
            continue;
        }
        let Ok(obj) = repo.odb.read(&oid) else {
            continue;
        };
        match obj.kind {
            ObjectKind::Commit => {
                if let Ok(c) = parse_commit(&obj.data) {
                    queue.push_back(c.tree);
                    for p in c.parents {
                        queue.push_back(p);
                    }
                }
            }
            ObjectKind::Tree => {
                if let Ok(entries) = parse_tree(&obj.data) {
                    for te in entries {
                        queue.push_back(te.oid);
                    }
                }
            }
            ObjectKind::Tag | ObjectKind::Blob => {}
        }
    }
    Ok(seen)
}

#[cfg(test)]
mod default_expire_tests {
    use std::fs;
    use std::path::Path;

    use tempfile::TempDir;

    use crate::config::ConfigSet;
    use crate::environment::Environment;
    use crate::objects::ObjectId;
    use crate::refs::{append_reflog, write_ref};
    use crate::repo::{init_repository, Repository};

    use super::{
        all_reflog_oids, all_reflog_oids_ordered, default_expire_total, default_expire_unreachable,
        delete_reflog_entries, expire_reflog, expire_reflog_git, list_reflog_refs,
        load_gc_reflog_expire_config, mark_stalefix_reachable, read_reflog, read_reflog_dwim,
        truncate_last_reflog_line, ReflogExpireActionKind, ReflogExpireParams,
    };
    use super::{parse_gc_reflog_expiry, parse_reflog_line};

    #[test]
    fn gc_reflog_expiry_uses_git_approxidate_threshold() {
        let now = 1_700_000_000_i64;
        assert_eq!(parse_gc_reflog_expiry("never", now).expect("never"), 0);
        assert_eq!(parse_gc_reflog_expiry("now", now).expect("now"), i64::MAX);
        // Git `match_digit`: bare integers >= 100_000_000 are Unix timestamps.
        assert_eq!(
            parse_gc_reflog_expiry("100000000", now).expect("100M"),
            100_000_000
        );
        // 86_401..99_999_999 are not literal epoch seconds (e.g. 90000 → approxidate, not 90000).
        assert_ne!(parse_gc_reflog_expiry("90000", now).expect("90000"), 90_000);
    }

    #[test]
    fn default_expire_total_is_ninety_days_before_now() {
        let now = 1_700_000_000_i64;
        let cutoff = default_expire_total(now);
        assert_eq!(cutoff, now - 90 * 86_400);
    }

    #[test]
    fn default_expire_unreachable_is_thirty_days_before_now() {
        let now = 1_700_000_000_i64;
        let cutoff = default_expire_unreachable(now);
        assert_eq!(cutoff, now - 30 * 86_400);
    }

    fn seed_repo() -> (TempDir, Repository) {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init");
        (tmp, repo)
    }

    fn write_loose_reflog(git_dir: &Path, refname: &str, body: &str) {
        let path = super::reflog_path(git_dir, refname);
        if let Some(p) = path.parent() {
            fs::create_dir_all(p).expect("mkdir logs");
        }
        fs::write(path, body).expect("write reflog");
    }

    #[test]
    fn parse_and_dwim_read_reflog_lines() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let oid = ObjectId::from_hex("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa").expect("oid");
        write_ref(git_dir, "refs/heads/dwim", &oid).expect("write ref");
        let line = format!(
            "0000000000000000000000000000000000000000 {oid} Tester <t@e.com> 100 +0000\tmsg\n"
        );
        write_loose_reflog(git_dir, "refs/heads/dwim", &line);
        let parsed = parse_reflog_line(line.trim_end()).expect("parse");
        assert_eq!(parsed.message, "msg");
        let dwim = read_reflog_dwim(git_dir, "dwim").expect("dwim");
        assert_eq!(dwim.len(), 1);
        assert_eq!(dwim[0].message, "msg");
        assert!(read_reflog(git_dir, "missing").expect("read").is_empty());
        let _ = tmp;
    }

    #[test]
    fn truncate_delete_and_expire_loose_reflog() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let refname = "refs/heads/main";
        let o1 = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("o1");
        let o2 = ObjectId::from_hex("cccccccccccccccccccccccccccccccccccccccc").expect("o2");
        write_ref(git_dir, refname, &o2).expect("tip");
        append_reflog(
            git_dir,
            refname,
            &ObjectId::zero(),
            &o1,
            "T <t@e.com> 1000 +0000",
            "a",
            true,
        )
        .expect("log1");
        append_reflog(
            git_dir,
            refname,
            &o1,
            &o2,
            "T <t@e.com> 2000 +0000",
            "b",
            false,
        )
        .expect("log2");

        delete_reflog_entries(git_dir, refname, &[0]).expect("delete newest");
        assert_eq!(read_reflog(git_dir, refname).expect("read").len(), 1);

        truncate_last_reflog_line(git_dir, refname).expect("truncate");
        assert!(read_reflog(git_dir, refname).expect("read2").is_empty());

        append_reflog(
            git_dir,
            refname,
            &ObjectId::zero(),
            &o1,
            "T <t@e.com> 500 +0000",
            "old",
            true,
        )
        .expect("log old");
        let pruned = expire_reflog(git_dir, refname, Some(1_000)).expect("expire");
        assert_eq!(pruned, 1);
        let _ = tmp;
    }

    #[test]
    fn expire_reflog_git_dry_run_and_stash_gc_never() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let now = 1_700_000_000_i64;
        let refname = "refs/stash";
        let oid = ObjectId::from_hex("dddddddddddddddddddddddddddddddddddddddd").expect("oid");
        write_ref(git_dir, refname, &oid).expect("stash ref");
        append_reflog(
            git_dir,
            refname,
            &ObjectId::zero(),
            &oid,
            &format!("T <t@e.com> {} +0000", now - 86_400),
            "stash",
            true,
        )
        .expect("stash log");

        let cfg = ConfigSet::load(&Environment::empty(), Some(git_dir), true).expect("cfg");
        let gc = load_gc_reflog_expire_config(&cfg, now);
        let dry = expire_reflog_git(
            &repo,
            git_dir,
            refname,
            &ReflogExpireParams {
                stale_fix: false,
                dry_run: true,
                verbose: true,
            },
            None,
            None,
            &gc.patterns,
            gc.global_total,
            gc.global_unreachable,
            now,
        )
        .expect("dry");
        assert!(!dry.actions.is_empty());
        assert!(read_reflog(git_dir, refname).expect("stash kept").len() == 1);

        let real = expire_reflog_git(
            &repo,
            git_dir,
            refname,
            &ReflogExpireParams {
                stale_fix: false,
                dry_run: false,
                verbose: false,
            },
            None,
            None,
            &gc.patterns,
            gc.global_total,
            gc.global_unreachable,
            now,
        )
        .expect("real");
        assert_eq!(real.pruned, 0, "refs/stash uses gc never by default");
        assert_eq!(read_reflog(git_dir, refname).expect("stash still").len(), 1);
        let _ = tmp;
    }

    #[test]
    fn mark_stalefix_reachable_walks_reflog_mentions() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let oid = ObjectId::from_hex("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee").expect("oid");
        write_ref(git_dir, "refs/heads/main", &oid).expect("write");
        append_reflog(
            git_dir,
            "refs/heads/main",
            &ObjectId::zero(),
            &oid,
            "T <t@e.com> 1000 +0000",
            "m",
            true,
        )
        .expect("log");
        let seen = mark_stalefix_reachable(&repo, git_dir).expect("stalefix");
        assert!(seen.contains(&oid));
        let _ = tmp;
    }

    #[test]
    fn all_reflog_oids_and_list_includes_head() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let oid = ObjectId::from_hex("1212121212121212121212121212121212121212").expect("oid");
        write_ref(git_dir, "refs/heads/main", &oid).expect("write");
        append_reflog(
            git_dir,
            "HEAD",
            &ObjectId::zero(),
            &oid,
            "T <t@e.com> 1000 +0000",
            "head",
            true,
        )
        .expect("head log");
        let refs = list_reflog_refs(git_dir).expect("list");
        assert!(refs.iter().any(|r| r == "HEAD"));
        let oids = all_reflog_oids(git_dir).expect("all oids");
        assert!(oids.contains(&oid));
        let ordered = all_reflog_oids_ordered(git_dir).expect("ordered");
        assert!(ordered.contains(&oid));
        let _ = tmp;
    }

    #[test]
    fn mirror_branch_reflog_to_head_copies_bytes() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let branch = "refs/heads/main";
        let oid = ObjectId::from_hex("1313131313131313131313131313131313131313").expect("oid");
        write_ref(git_dir, branch, &oid).expect("write");
        append_reflog(
            git_dir,
            branch,
            &ObjectId::zero(),
            &oid,
            "T <t@e.com> 1000 +0000",
            "branch",
            true,
        )
        .expect("branch log");
        super::mirror_branch_reflog_to_head(git_dir, branch).expect("mirror");
        let head = read_reflog(git_dir, "HEAD").expect("head log");
        assert_eq!(head.len(), 1);
        assert_eq!(head[0].message, "branch");
        let _ = tmp;
    }

    #[test]
    fn read_reflog_skips_blank_lines_and_no_message_tab() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let oid = ObjectId::from_hex("1414141414141414141414141414141414141414").expect("oid");
        let body =
            format!("\n0000000000000000000000000000000000000000 {oid} T <t@e.com> 1000 +0000\n");
        write_loose_reflog(git_dir, "refs/heads/x", &body);
        let entries = read_reflog(git_dir, "refs/heads/x").expect("read");
        assert_eq!(entries.len(), 1);
        assert!(entries[0].message.is_empty());
        let _ = tmp;
    }

    #[test]
    fn expire_git_prunes_old_entry_and_reports_action() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir;
        let refname = "refs/heads/main";
        let oid = ObjectId::from_hex("ffffffffffffffffffffffffffffffffffffffff").expect("oid");
        write_ref(git_dir, refname, &oid).expect("write");
        append_reflog(
            git_dir,
            refname,
            &ObjectId::zero(),
            &oid,
            "T <t@e.com> 1000 +0000",
            "prune-me",
            true,
        )
        .expect("log");
        let now = 2_000_000_i64;
        let cfg = ConfigSet::load(&Environment::empty(), Some(git_dir), true).expect("cfg");
        let gc = load_gc_reflog_expire_config(&cfg, now);
        let result = expire_reflog_git(
            &repo,
            git_dir,
            refname,
            &ReflogExpireParams {
                stale_fix: false,
                dry_run: false,
                verbose: false,
            },
            Some(1_500_000),
            None,
            &gc.patterns,
            gc.global_total,
            gc.global_unreachable,
            now,
        )
        .expect("expire");
        assert_eq!(result.pruned, 1);
        assert!(result
            .actions
            .iter()
            .any(|a| a.action == ReflogExpireActionKind::Prune && a.entry.message == "prune-me"));
        assert!(read_reflog(git_dir, refname).expect("empty").is_empty());
        let _ = tmp;
    }
}
