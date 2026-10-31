//! Reflog reading and management.
//!
//! The reflog records updates to refs.  Each ref's log is stored at
//! `<git-dir>/logs/<refname>` (e.g. `logs/HEAD`, `logs/refs/heads/main`).
//! Each line has the format:
//!
//! ```text
//! <old-sha> <new-sha> <name> <<email>> <timestamp> <timezone>\t<message>
//! ```

use std::cell::{OnceCell, RefCell};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::fs::File;
use std::io;
use std::io::{Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::commit_graph_file::CommitGraphChain;
use crate::config::ConfigSet;
use crate::error::{Error, Result};
use crate::ident::Signature;
use crate::index::MODE_TREE;
use crate::merge_base;
use crate::objects::{parse_commit, parse_tree, CommitRef, ObjectId, ObjectKind, OidHashMap};
use crate::refs::store::{RefStore as _, RefStoreError};
use crate::refs::{self, reflog_file_path};
use crate::repo::Repository;
use crate::wildmatch::{wildmatch, WM_PATHNAME};

mod selector;
pub use selector::{parse_reflog_ref, ReflogSelector};

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

impl ReflogEntry {
    /// Parsed committer identity and timestamp from [`Self::identity`].
    ///
    /// Returns [`None`] when the identity line is malformed (no sentinel timestamp).
    #[must_use]
    pub fn signature(&self) -> Option<Signature> {
        crate::ident::parse_signature(&self.identity)
    }

    /// Best-effort action prefix parsed from [`Self::message`].
    ///
    /// Returns the substring before the first `": "` when present (for example `commit`,
    /// `commit (amend)`, `checkout`, `reset`, `rebase (finish)`, `merge branch`).
    /// Reflog messages are free-form text; callers must not rely on this for correctness.
    #[must_use]
    pub fn action(&self) -> Option<&str> {
        self.message.split_once(": ").map(|(prefix, _)| prefix)
    }
}

/// Case-sensitive substring filter for reflog message text (`--grep`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MessageMatcher {
    needle: String,
}

impl MessageMatcher {
    /// Build a matcher that succeeds when the message contains `pattern` verbatim.
    #[must_use]
    pub fn new(pattern: impl Into<String>) -> Self {
        Self {
            needle: pattern.into(),
        }
    }

    /// Returns whether `message` contains the configured pattern.
    #[must_use]
    pub fn matches(&self, message: &str) -> bool {
        message.contains(&self.needle)
    }
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
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
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
    if let Ok(store) = crate::refs::store::open_ref_store(git_dir) {
        if store.format() != crate::refs::store::RefStorageFormat::Files {
            return store.reflog_exists(refname).unwrap_or(false);
        }
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
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
        let store = crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?;
        let mut entries = Vec::new();
        store.for_each_reflog_entry(refname, false, &mut |entry| {
            entries.push(entry.clone());
            std::ops::ControlFlow::Continue(())
        })?;
        return Ok(entries);
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

/// Count reflog entries for `refname` without parsing or retaining them.
///
/// Returns `0` when the reflog file is missing.
pub(crate) fn reflog_entry_count(git_dir: &Path, refname: &str) -> Result<usize> {
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
        let store = crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?;
        let mut count = 0usize;
        store.for_each_reflog_entry(refname, false, &mut |_| {
            count += 1;
            std::ops::ControlFlow::Continue(())
        })?;
        return Ok(count);
    }
    let path = reflog_path(git_dir, refname);
    let content = match fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Error::Io(e)),
    };
    Ok(content.lines().filter(|line| !line.is_empty()).count())
}

const REFLOG_REVERSE_READ_CHUNK: usize = 64 * 1024;

/// Reads a loose reflog file from the end in fixed-size chunks (newest line first).
pub(crate) struct ReverseReflogLineReader {
    file: File,
    loaded_from: u64,
    buffer: Vec<u8>,
}

impl ReverseReflogLineReader {
    /// Opens an existing non-empty reflog file for reverse line iteration.
    ///
    /// # Errors
    ///
    /// Propagates I/O errors other than a missing log (a missing path, or a directory in
    /// the way, yields `Ok(None)`).
    pub fn open(path: &Path) -> Result<Option<Self>> {
        let file = match File::open(path) {
            Ok(f) => f,
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
                ) =>
            {
                return Ok(None)
            }
            Err(e) => return Err(Error::Io(e)),
        };
        let meta = file.metadata().map_err(Error::Io)?;
        let len = meta.len();
        if !meta.is_file() || len == 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            file,
            loaded_from: len,
            buffer: Vec::new(),
        }))
    }

    /// Returns the next non-empty line's raw bytes, newest first.
    ///
    /// # Errors
    ///
    /// Propagates I/O failures while reading.
    pub fn next_nonempty_line(&mut self) -> Result<Option<Vec<u8>>> {
        loop {
            if let Some(line) = take_newest_line(&mut self.buffer)? {
                if line.is_empty() || line == b"\r" {
                    continue;
                }
                return Ok(Some(line));
            }
            if self.loaded_from == 0 {
                if self.buffer.is_empty() {
                    return Ok(None);
                }
                let mut line = std::mem::take(&mut self.buffer);
                while line.last() == Some(&b'\r') {
                    line.pop();
                }
                if line.is_empty() {
                    return Ok(None);
                }
                return Ok(Some(line));
            }
            let read_end = self.loaded_from;
            let read_start = read_end.saturating_sub(REFLOG_REVERSE_READ_CHUNK as u64);
            let read_len = (read_end - read_start) as usize;
            self.file
                .seek(SeekFrom::Start(read_start))
                .map_err(Error::Io)?;
            let mut chunk = vec![0u8; read_len];
            self.file.read_exact(&mut chunk).map_err(Error::Io)?;
            self.loaded_from = read_start;
            let mut merged = chunk;
            merged.append(&mut self.buffer);
            self.buffer = merged;
        }
    }
}

fn take_newest_line(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    loop {
        if buffer.is_empty() {
            return Ok(None);
        }
        let Some(newline_at) = buffer.iter().rposition(|&b| b == b'\n') else {
            return Ok(None);
        };
        let mut line = buffer.split_off(newline_at + 1);
        buffer.truncate(newline_at);
        while line.last() == Some(&b'\r') {
            line.pop();
        }
        if line.is_empty() {
            continue;
        }
        return Ok(Some(line));
    }
}

/// Parse a single reflog line.
///
/// Format: `<old-hex> <new-hex> <identity>\t<message>`
/// Parse one reflog line (for embedders that read log files directly).
pub(crate) fn parse_reflog_line_for_store(line: &str) -> Option<ReflogEntry> {
    parse_reflog_line(line)
}

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

/// Collect every non-null object ID mentioned in any reflog.
///
/// Used by `fsck` to validate reflog entries across loose-file and reftable backends.
pub fn all_reflog_oids(git_dir: &Path) -> Result<HashSet<ObjectId>> {
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
        return Ok(all_reflog_oids_ordered(git_dir)?.into_iter().collect());
    }
    let mut out = HashSet::new();
    let logs = git_dir.join("logs");
    if !logs.is_dir() {
        return Ok(out);
    }
    walk_reflog_files(&logs, &mut out)?;
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
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
        // Reftable repos: fall back to the ref-name-ordered reflog walk for a deterministic order.
        let mut out = Vec::new();
        let mut seen = HashSet::new();
        let mut names = list_reflog_refs(git_dir).unwrap_or_default();
        names.sort();
        for refname in names {
            for entry in read_reflog(git_dir, &refname).unwrap_or_default() {
                for oid in [entry.old_oid, entry.new_oid] {
                    if !oid.is_zero() && seen.insert(oid) {
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
                if !oid.is_zero() && seen.insert(oid) {
                    out.push(oid);
                }
            }
        }
    }
    Ok(out)
}

fn walk_reflog_files(dir: &Path, out: &mut HashSet<ObjectId>) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(Error::Io)? {
        let entry = entry.map_err(Error::Io)?;
        let path = entry.path();
        if path.is_dir() {
            walk_reflog_files(&path, out)?;
        } else if path.is_file() {
            let content = fs::read_to_string(&path).map_err(Error::Io)?;
            for line in content.lines() {
                if let Some(e) = parse_reflog_line(line) {
                    if !e.old_oid.is_zero() {
                        out.insert(e.old_oid);
                    }
                    if !e.new_oid.is_zero() {
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
/// Rewrites the reflog through the ref store for `git_dir`, omitting entries at the given
/// indices.
pub fn delete_reflog_entries(git_dir: &Path, refname: &str, indices: &[usize]) -> Result<()> {
    let store = crate::refs::store::open_ref_store(git_dir)?;
    delete_reflog_entries_on_store(store.as_ref(), refname, indices)
}

/// Oldest-first `entries` without those at the newest-first `indices`, plus whether the
/// oldest entry was among the removed ones. `None` when `entries` is empty.
fn filter_reflog_without_indices(
    entries: Vec<ReflogEntry>,
    indices: &[usize],
) -> Option<(Vec<ReflogEntry>, bool)> {
    if entries.is_empty() {
        return None;
    }
    let len = entries.len();
    let indices_set: HashSet<usize> = indices.iter().copied().collect();
    let removed_oldest = indices_set.contains(&(len - 1));
    let kept = entries
        .into_iter()
        .enumerate()
        .filter(|(oldest_first, _)| !indices_set.contains(&(len - 1 - oldest_first)))
        .map(|(_, e)| e)
        .collect();
    Some((kept, removed_oldest))
}

fn rechain_reflog_after_delete(
    mut kept: Vec<ReflogEntry>,
    removed_oldest: bool,
) -> Vec<ReflogEntry> {
    if removed_oldest {
        if let Some(first) = kept.first_mut() {
            first.old_oid = ObjectId::zero();
        }
    }
    for i in 1..kept.len() {
        kept[i].old_oid = kept[i - 1].new_oid;
    }
    kept
}

/// [`delete_reflog_entries`] through an already-selected ref store (for example
/// [`crate::repo::Repository::refs`], which may be an injected store).
pub(crate) fn delete_reflog_entries_on_store(
    store: &dyn crate::refs::store::RefStore,
    refname: &str,
    indices: &[usize],
) -> Result<()> {
    let indices: Vec<usize> = indices.to_vec();
    store.rewrite_reflog(refname, &mut move |current| {
        Ok(filter_reflog_without_indices(current, &indices)
            .map(|(kept, _)| kept)
            .unwrap_or_default())
    })
}

/// Like [`delete_reflog_entries_on_store`], but reconnects surviving `old_oid` values after
/// a gap (Git stash drop / `EXPIRE_REFLOGS_REWRITE`).
pub(crate) fn delete_reflog_entries_rechain_on_store(
    store: &dyn crate::refs::store::RefStore,
    refname: &str,
    indices: &[usize],
) -> Result<()> {
    let indices: Vec<usize> = indices.to_vec();
    store.rewrite_reflog(refname, &mut move |current| {
        Ok(filter_reflog_without_indices(current, &indices)
            .map(|(kept, removed_oldest)| rechain_reflog_after_delete(kept, removed_oldest))
            .unwrap_or_default())
    })
}

/// Read every reflog entry for `refname` from `store`, oldest first; empty when it has none.
///
/// # Errors
///
/// Propagates backend read and parse failures.
pub(crate) fn read_reflog_on_store(
    store: &dyn crate::refs::store::RefStore,
    refname: &str,
) -> Result<Vec<ReflogEntry>> {
    if let Some(files) = store.as_files() {
        return files.read_reflog_entries(refname);
    }
    let mut entries = Vec::new();
    store.for_each_reflog_entry(refname, false, &mut |entry| {
        entries.push(entry.clone());
        std::ops::ControlFlow::Continue(())
    })?;
    Ok(entries)
}

/// Expire (prune) reflog entries older than a given timestamp (Unix seconds).
///
/// If `expire_time` is `None`, removes all entries.
pub fn expire_reflog(git_dir: &Path, refname: &str, expire_time: Option<i64>) -> Result<usize> {
    let store = crate::refs::store::open_ref_store(git_dir)?;
    let mut pruned = 0usize;
    store.rewrite_reflog(refname, &mut |current| {
        if current.is_empty() {
            return Ok(current);
        }
        let mut kept = Vec::with_capacity(current.len());
        for entry in current {
            let ts = parse_timestamp_from_identity(&entry.identity);
            let dominated = match (expire_time, ts) {
                (Some(cutoff), Some(t)) => t < cutoff,
                (None, _) => true,
                (Some(_), None) => false,
            };
            if dominated {
                pruned += 1;
            } else {
                kept.push(entry);
            }
        }
        Ok(kept)
    })?;
    Ok(pruned)
}

/// Expire reflog entries whose `new_oid` is not an ancestor of the current ref tip
/// and whose identity timestamp is older than `cutoff` (Unix seconds).
///
/// Entries with an all-zero `new_oid` are never removed by this pass.
///
/// When `cutoff` is `None`, no entries are removed.
///
pub fn expire_reflog_unreachable(
    repo: &Repository,
    _git_dir: &Path,
    refname: &str,
    cutoff: Option<i64>,
) -> Result<usize> {
    let Some(cutoff) = cutoff else {
        return Ok(0);
    };
    let tip = match refs::resolve_ref_in(repo, refname) {
        Ok(o) => o,
        Err(_) => return Ok(0),
    };
    let ancestors = match merge_base::ancestor_closure(repo, tip) {
        Ok(a) => a,
        Err(_) => return Ok(0),
    };

    let store = repo.refs();
    let snapshot = read_reflog_on_store(store, refname)?;
    if snapshot.is_empty() {
        return Ok(0);
    }

    let drop_flags: Vec<bool> = snapshot
        .iter()
        .map(|entry| {
            let ts = parse_timestamp_from_identity(&entry.identity);
            let unreachable = !entry.new_oid.is_zero() && !ancestors.contains(&entry.new_oid);
            unreachable && matches!(ts, Some(t) if t < cutoff)
        })
        .collect();
    let pruned = drop_flags.iter().filter(|&&drop| drop).count();
    if pruned == 0 {
        return Ok(0);
    }
    rewrite_reflog_from_snapshot(store, refname, &snapshot, &drop_flags)?;
    Ok(pruned)
}

/// Extract the Unix timestamp from an identity string.
///
/// Identity format: `Name <email> <timestamp> <tz>`
/// Unix timestamp from a reflog identity line (`Name <email> <ts> <tz>`).
pub fn reflog_identity_timestamp(identity: &str) -> Option<i64> {
    parse_timestamp_from_identity(identity)
}

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
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
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

/// Names of every ref with a reflog in `store`, sorted.
///
/// # Errors
///
/// Propagates backend read failures.
pub(crate) fn reflog_refs_on_store(
    store: &dyn crate::refs::store::RefStore,
) -> Result<Vec<String>> {
    let mut names = Vec::new();
    store.for_each_reflog_ref(&mut |name| {
        names.push(name.to_owned());
        ControlFlow::Continue(())
    })?;
    names.sort();
    names.dedup();
    Ok(names)
}

/// List all refs that have reflogs.
pub fn list_reflog_refs(git_dir: &Path) -> Result<Vec<String>> {
    if crate::refs::store::open_ref_store(git_dir)?.format()
        != crate::refs::store::RefStorageFormat::Files
    {
        let store = crate::refs::store::ReftableRefStore::open(git_dir.to_path_buf())?;
        let mut refs = Vec::new();
        store.for_each_reflog_ref(&mut |name| {
            refs.push(name.to_owned());
            std::ops::ControlFlow::Continue(())
        })?;
        refs.sort();
        return Ok(refs);
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
        let refs_logs = crate::refs::store::paths::logs_refs_from_logs_root(logs_dir);
        if refs_logs.is_dir() {
            collect_reflog_refs(&refs_logs, "refs", out, seen, skip_per_worktree_refs)?;
        }
        Ok(())
    }

    collect_from_logs_root(
        &crate::refs::store::paths::logs_dir_path(git_dir),
        &mut refs,
        &mut seen,
        false,
    )?;
    if let Some(common) = refs::common_dir(git_dir) {
        if common != git_dir {
            collect_from_logs_root(
                &crate::refs::store::paths::logs_dir_path(&common),
                &mut refs,
                &mut seen,
                true,
            )?;
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
    /// Also prune entries whose commits, or any commit, tree or blob reachable from them, are
    /// missing. This walks full history once per expiry; ordinary expiry never does.
    pub stale_fix: bool,
    /// `dry_run` field.
    pub dry_run: bool,
    /// `verbose` field.
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
    let mut pattern_order: Vec<String> = Vec::new();
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
        let Ok(ts) = parse_reflog_expire_spec(val, now) else {
            continue;
        };
        let pat_owned = pat.to_string();
        if !by_pattern.contains_key(&pat_owned) {
            pattern_order.push(pat_owned.clone());
        }
        let ent = by_pattern.entry(pat_owned).or_insert(GcReflogPattern {
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
    pattern_order
        .into_iter()
        .filter_map(|p| by_pattern.remove(&p))
        .collect()
}

fn global_gc_reflog_expiry(config: &ConfigSet, now: i64) -> (Option<i64>, Option<i64>) {
    let total = config
        .get("gc.reflogExpire")
        .and_then(|v| parse_reflog_expire_spec(&v, now).ok());
    let unreach = config
        .get("gc.reflogExpireUnreachable")
        .and_then(|v| parse_reflog_expire_spec(&v, now).ok());
    (total, unreach)
}

/// Parse `gc.reflogExpire` / `--expire` values: `never` / `false` → keep forever (`0`), else days or epoch.
pub fn parse_reflog_expire_spec(raw: &str, now: i64) -> Result<i64> {
    let s = raw.trim();
    if s.eq_ignore_ascii_case("never") || s.eq_ignore_ascii_case("false") {
        return Ok(0);
    }
    if s.eq_ignore_ascii_case("now") || s.eq_ignore_ascii_case("all") {
        return Ok(i64::MAX);
    }
    crate::git_date::parse_date_cutoff(s, now)
        .map_err(|_| Error::Message(format!("invalid reflog expiry: {raw:?}")))
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
        if wildmatch(ent.pattern.as_bytes(), refname.as_bytes(), WM_PATHNAME)
            || wildmatch(ent.pattern.as_bytes(), refname.as_bytes(), 0)
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

/// Lazily computed state shared by the reflog expiry of several refs in one pass.
///
/// Every member is only filled when an expiry needs it: the promise boundary when a tip has
/// to be classified or `--stale-fix` runs, commit linkage when old entries are judged by
/// reachability, and the completeness answers only for `--stale-fix`.
#[derive(Debug, Default)]
pub(crate) struct ReflogExpiryCache {
    promised: OnceCell<HashSet<ObjectId>>,
    /// Every commit a reachability walk has reached, with its parents once read.
    commits: RefCell<CommitLinks>,
    complete: RefCell<HashMap<ObjectId, bool>>,
}

/// Commits interned to dense indices, so the reachability walk of each ref marks a bit per
/// commit instead of hashing object ids, and later refs in the pass reuse earlier reads.
#[derive(Debug, Default)]
struct CommitLinks {
    index: OidHashMap<u32>,
    nodes: Vec<Linkage>,
}

#[derive(Debug)]
enum Linkage {
    Unread(ObjectId),
    /// Missing, or not a commit.
    Absent,
    Read {
        time: i64,
        parents: Box<[u32]>,
    },
}

impl CommitLinks {
    fn intern(&mut self, oid: ObjectId) -> u32 {
        let next = u32::try_from(self.nodes.len()).unwrap_or(u32::MAX);
        let node = *self.index.entry(oid).or_insert(next);
        if node == next {
            self.nodes.push(Linkage::Unread(oid));
        }
        node
    }

    fn lookup(&self, oid: &ObjectId) -> Option<u32> {
        self.index.get(oid).copied()
    }

    fn len(&self) -> usize {
        self.nodes.len()
    }

    /// Committer time and parents of `node`, calling `read` for the parents and committer time
    /// of a commit the pass has not read yet. `None` when the commit is missing or not a commit.
    fn linkage(
        &mut self,
        node: u32,
        read: impl FnOnce(ObjectId) -> Option<(Vec<ObjectId>, i64)>,
    ) -> Option<(i64, &[u32])> {
        let idx = node as usize;
        if let Linkage::Unread(oid) = self.nodes[idx] {
            self.nodes[idx] = match read(oid) {
                Some((parents, time)) => Linkage::Read {
                    time,
                    parents: parents.into_iter().map(|p| self.intern(p)).collect(),
                },
                None => Linkage::Absent,
            };
        }
        match &self.nodes[idx] {
            Linkage::Read { time, parents } => Some((*time, parents)),
            Linkage::Unread(_) | Linkage::Absent => None,
        }
    }
}

impl ReflogExpiryCache {
    /// Partial-clone promises, read without fetching; missing promised objects count as
    /// present everywhere expiry looks at objects.
    fn promised(&self, repo: &Repository) -> Result<&HashSet<ObjectId>> {
        if let Some(promised) = self.promised.get() {
            return Ok(promised);
        }
        let promised = crate::promisor::maintenance_promise_boundary(repo.odb())?;
        Ok(self.promised.get_or_init(|| promised))
    }

    /// Whether `root` and every commit, tree and blob reachable from it is present (or
    /// promised).
    ///
    /// One iterative walk memoises every commit and tree it finishes, so history shared
    /// between reflog entries and refs is read once per expiry pass, merge diamonds are not
    /// re-walked, and deep history cannot overflow the stack.
    fn commit_complete(
        &self,
        repo: &Repository,
        promised: &HashSet<ObjectId>,
        root: ObjectId,
    ) -> bool {
        enum Step {
            Enter(ObjectId, ObjectKind),
            Finish(ObjectId, Vec<ObjectId>),
        }
        let is_boundary = |oid: &ObjectId| oid.is_zero() || promised.contains(oid);
        if is_boundary(&root) {
            return true;
        }
        let mut memo = self.complete.borrow_mut();
        let mut stack = vec![Step::Enter(root, ObjectKind::Commit)];
        while let Some(step) = stack.pop() {
            match step {
                Step::Enter(oid, kind) => {
                    if is_boundary(&oid) || memo.contains_key(&oid) {
                        continue;
                    }
                    // Provisionally incomplete: an object reached again before it finishes
                    // would be its own ancestor, which only a corrupt store can serve.
                    memo.insert(oid, false);
                    let Some(children) = object_children(repo, promised, oid, kind) else {
                        continue;
                    };
                    let next = children
                        .iter()
                        .map(|&(child, kind)| Step::Enter(child, kind))
                        .collect::<Vec<_>>();
                    stack.push(Step::Finish(
                        oid,
                        children.into_iter().map(|(child, _)| child).collect(),
                    ));
                    stack.extend(next);
                }
                Step::Finish(oid, children) => {
                    let complete = children
                        .iter()
                        .all(|child| is_boundary(child) || memo.get(child) == Some(&true));
                    memo.insert(oid, complete);
                }
            }
        }
        memo.get(&root) == Some(&true)
    }
}

/// The commits and trees `oid` (expected to be a `kind`) points to, or `None` when `oid` is
/// missing, of another kind, unparsable, or a tree naming a missing non-tree entry.
///
/// Non-tree entries are only checked for existence, so blobs are never inflated.
fn object_children(
    repo: &Repository,
    promised: &HashSet<ObjectId>,
    oid: ObjectId,
    kind: ObjectKind,
) -> Option<Vec<(ObjectId, ObjectKind)>> {
    let obj = repo.odb().try_read_once(&oid).ok()??;
    if obj.kind != kind {
        return None;
    }
    match kind {
        ObjectKind::Commit => {
            let commit = parse_commit(&obj.data).ok()?;
            Some(
                std::iter::once((commit.tree, ObjectKind::Tree))
                    .chain(commit.parents.into_iter().map(|p| (p, ObjectKind::Commit)))
                    .collect(),
            )
        }
        ObjectKind::Tree => {
            let mut subtrees = Vec::new();
            for entry in parse_tree(&obj.data).ok()? {
                if entry.mode == MODE_TREE {
                    subtrees.push((entry.oid, ObjectKind::Tree));
                } else if !promised.contains(&entry.oid) && !repo.odb().exists(&entry.oid) {
                    return None;
                }
            }
            Some(subtrees)
        }
        ObjectKind::Blob | ObjectKind::Tag => None,
    }
}

/// The commit a ref tip names after peeling annotated tags, read without lazy fetching.
///
/// Returns `None` for a zero id, a missing object or a tip that is not a commit; promised
/// objects count as present commits so partial clones stay offline.
fn peel_tip_to_commit(
    repo: &Repository,
    promised: &HashSet<ObjectId>,
    oid: ObjectId,
) -> Option<ObjectId> {
    if oid.is_zero() {
        return None;
    }
    if promised.contains(&oid) {
        return Some(oid);
    }
    let target = repo
        .odb()
        .peel_stored_tag_target(oid)
        .ok()
        .flatten()
        .unwrap_or(oid);
    if promised.contains(&target) {
        return Some(target);
    }
    match repo.odb().try_read_once(&target) {
        Ok(Some(obj)) if obj.kind == ObjectKind::Commit => Some(target),
        _ => None,
    }
}

fn is_head_ref(refname: &str) -> bool {
    refname == "HEAD" || refname.ends_with("/HEAD")
}

/// Every ref tip that peels to a commit: the starting points for the reachability check of
/// a `HEAD` reflog.
fn head_reflog_tips(repo: &Repository, promised: &HashSet<ObjectId>) -> Vec<ObjectId> {
    let store = repo.refs();
    let mut tips = Vec::new();
    if let Ok(oid) = store.resolve("HEAD") {
        tips.push(oid);
    }
    if let Ok(refs) = refs::collect_resolved_refs(store, "refs/") {
        tips.extend(refs.into_iter().map(|(_, oid)| oid));
    }
    tips.sort();
    tips.dedup();
    tips.into_iter()
        .filter_map(|oid| peel_tip_to_commit(repo, promised, oid))
        .collect()
}

/// Commits reachable from a set of tips, discovered lazily.
///
/// The first walk stops below commits older than a cutoff (the total-expiry cutoff, which
/// old reflog entries rarely reach past); only a lookup that misses walks the rest of
/// history, once. Answers never depend on the cutoff, only the amount of history read.
/// Shallow boundaries are not walked through. Parents come from the commit-graph when it
/// covers a commit, and are kept in the [`ReflogExpiryCache`] so the walks of later refs in
/// the same pass do not read them again; each walk only marks the interned commits it visits.
struct ReachableCommits<'r> {
    repo: &'r Repository,
    cache: &'r ReflogExpiryCache,
    graph: Option<Arc<CommitGraphChain>>,
    /// Interned shallow boundary commits, sorted.
    shallow: Vec<u32>,
    /// Indexed by interned commit; grows as the walk interns new commits.
    seen: Vec<bool>,
    seen_count: usize,
    pending: Vec<u32>,
    /// Parents of commits older than the cutoff, walked on the first miss.
    deferred: Vec<u32>,
}

impl<'r> ReachableCommits<'r> {
    /// Walk from `tips` down to commits older than `cutoff` (`0` walks everything).
    fn new(
        repo: &'r Repository,
        cache: &'r ReflogExpiryCache,
        tips: Vec<ObjectId>,
        cutoff: i64,
    ) -> Self {
        let mut reachable = Self {
            repo,
            cache,
            graph: None,
            shallow: Vec::new(),
            seen: Vec::new(),
            seen_count: 0,
            pending: Vec::new(),
            deferred: Vec::new(),
        };
        if tips.is_empty() {
            return reachable;
        }
        reachable.graph = repo.caches().commit_graph_chain(repo.odb().objects_dir());
        {
            let mut commits = cache.commits.borrow_mut();
            let mut shallow = crate::shallow::load_shallow_boundaries(repo.git_dir())
                .into_iter()
                .map(|oid| commits.intern(oid))
                .collect::<Vec<_>>();
            shallow.sort_unstable();
            reachable.shallow = shallow;
            reachable.pending = tips.into_iter().map(|oid| commits.intern(oid)).collect();
        }
        reachable.walk(cutoff);
        reachable
    }

    fn walk(&mut self, cutoff: i64) {
        let repo = self.repo;
        let graph = self.graph.as_deref();
        let mut commits = self.cache.commits.borrow_mut();
        while let Some(node) = self.pending.pop() {
            let idx = node as usize;
            if idx >= self.seen.len() {
                self.seen.resize(commits.len(), false);
            }
            if std::mem::replace(&mut self.seen[idx], true) {
                continue;
            }
            self.seen_count += 1;
            if !self.shallow.is_empty() && self.shallow.binary_search(&node).is_ok() {
                continue;
            }
            let Some((time, parents)) =
                commits.linkage(node, |oid| read_commit_linkage(repo, graph, oid))
            else {
                continue;
            };
            if cutoff > 0 && time < cutoff {
                self.deferred.extend_from_slice(parents);
            } else {
                self.pending.extend_from_slice(parents);
            }
        }
    }

    fn is_seen(&self, oid: &ObjectId) -> bool {
        self.cache
            .commits
            .borrow()
            .lookup(oid)
            .is_some_and(|node| self.seen.get(node as usize) == Some(&true))
    }

    fn contains(&mut self, oid: &ObjectId) -> bool {
        if !self.is_seen(oid) && !self.deferred.is_empty() {
            self.pending.append(&mut self.deferred);
            self.walk(0);
        }
        self.is_seen(oid)
    }
}

/// Parents and committer time of commit `oid`, from the commit-graph when it covers `oid`, or
/// `None` when it is missing or not a commit.
fn read_commit_linkage(
    repo: &Repository,
    graph: Option<&CommitGraphChain>,
    oid: ObjectId,
) -> Option<(Vec<ObjectId>, i64)> {
    if let Some(linkage) = graph.and_then(|g| g.graph_commit(&oid)) {
        return Some(linkage);
    }
    let obj = repo.odb().try_read_once(&oid).ok()??;
    if obj.kind != ObjectKind::Commit {
        return None;
    }
    let commit = CommitRef::parse(&obj.data).ok()?;
    Some((commit.parents().to_vec(), commit.committer_timestamp()))
}

/// Whether a reflog entry side names a commit that is not reachable. Missing objects count
/// as unreachable, and objects that are not commits as reachable.
fn is_unreachable_oid(
    repo: &Repository,
    reachable: &mut ReachableCommits<'_>,
    oid: ObjectId,
) -> bool {
    if oid.is_zero() || reachable.is_seen(&oid) {
        return false;
    }
    match repo.odb().try_read_once(&oid) {
        Ok(Some(obj)) if obj.kind == ObjectKind::Commit => !reachable.contains(&oid),
        Ok(Some(_)) => false,
        _ => true,
    }
}

fn reflog_entry_timestamp(entry: &ReflogEntry) -> i64 {
    parse_timestamp_from_identity(&entry.identity).unwrap_or(i64::MAX)
}

/// The `--stale-fix` completeness check, present only when `--stale-fix` was requested.
struct StaleFix<'a> {
    cache: &'a ReflogExpiryCache,
    promised: &'a HashSet<ObjectId>,
}

impl StaleFix<'_> {
    fn entry_is_stale(&self, repo: &Repository, entry: &ReflogEntry) -> bool {
        !self
            .cache
            .commit_complete(repo, self.promised, entry.old_oid)
            || !self
                .cache
                .commit_complete(repo, self.promised, entry.new_oid)
    }
}

fn should_drop_reflog_entry(
    repo: &Repository,
    entry: &ReflogEntry,
    expire_total: i64,
    expire_unreachable: i64,
    reachable: Option<&mut ReachableCommits<'_>>,
    stale_fix: Option<&StaleFix<'_>>,
) -> bool {
    let ts = reflog_entry_timestamp(entry);
    if expire_total > 0 && ts < expire_total {
        return true;
    }
    if stale_fix.is_some_and(|fix| fix.entry_is_stale(repo, entry)) {
        return true;
    }
    if expire_unreachable == 0 || ts >= expire_unreachable {
        return false;
    }
    // Without a reachability set (no commit tip to judge by) every old entry expires.
    let Some(reachable) = reachable else {
        return true;
    };
    is_unreachable_oid(repo, reachable, entry.old_oid)
        || is_unreachable_oid(repo, reachable, entry.new_oid)
}

/// Reflog expiry for one ref.
///
/// Entries older than the total cutoff are dropped. Entries older than the unreachable
/// cutoff are dropped when their old or new commit is not reachable from the ref's tip (from
/// any ref for a `HEAD` reflog); that check is one commit-only walk per ref, and when the tip
/// does not peel to a commit every such entry is dropped, as in Git. Only
/// [`ReflogExpireParams::stale_fix`] additionally checks that the full history and trees of
/// each entry are present, in one memoised walk. Neither check fetches missing objects:
/// partial-clone promises count as present.
///
/// The reflog is read from and rewritten through [`Repository::refs`], and ref tips for the
/// reachability check are resolved there too, so an injected ref store is expired like the
/// built-in backends. A dry run writes nothing. On the files backend a fully pruned log is
/// kept as an empty file and gets `core.sharedRepository` permissions, as Git does.
///
/// # Errors
///
/// Propagates ref-store read and write failures and promisor-boundary lookup failures.
#[expect(clippy::too_many_arguments)]
pub fn expire_reflog_git(
    repo: &Repository,
    refname: &str,
    params: &ReflogExpireParams,
    explicit_total: Option<i64>,
    explicit_unreachable: Option<i64>,
    gc_patterns: &[GcReflogPattern],
    gc_global_total: Option<i64>,
    gc_global_unreachable: Option<i64>,
    now: i64,
) -> Result<ReflogExpireResult> {
    expire_reflog_git_with_cache(
        repo,
        refname,
        params,
        explicit_total,
        explicit_unreachable,
        gc_patterns,
        gc_global_total,
        gc_global_unreachable,
        now,
        &ReflogExpiryCache::default(),
    )
}

/// [`expire_reflog_git`] sharing promise classification and stale-fix completeness answers
/// across all refs of one expiry pass.
#[expect(clippy::too_many_arguments)]
pub(crate) fn expire_reflog_git_with_cache(
    repo: &Repository,
    refname: &str,
    params: &ReflogExpireParams,
    explicit_total: Option<i64>,
    explicit_unreachable: Option<i64>,
    gc_patterns: &[GcReflogPattern],
    gc_global_total: Option<i64>,
    gc_global_unreachable: Option<i64>,
    now: i64,
    cache: &ReflogExpiryCache,
) -> Result<ReflogExpireResult> {
    let store = repo.refs();
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

    let entries = read_reflog_on_store(store, refname)?;
    if entries.is_empty() {
        return Ok(ReflogExpireResult {
            pruned: 0,
            actions: Vec::new(),
        });
    }

    let may_drop_total = expire_total > 0
        && entries
            .iter()
            .any(|e| reflog_entry_timestamp(e) < expire_total);
    let may_drop_unreachable = expire_unreachable > 0
        && entries
            .iter()
            .any(|e| reflog_entry_timestamp(e) < expire_unreachable);
    let stale_fix = if params.stale_fix {
        Some(StaleFix {
            cache,
            promised: cache.promised(repo)?,
        })
    } else {
        None
    };
    let may_drop_stale = stale_fix
        .as_ref()
        .is_some_and(|fix| entries.iter().any(|e| fix.entry_is_stale(repo, e)));

    if !may_drop_total && !may_drop_stale && !may_drop_unreachable {
        return Ok(ReflogExpireResult {
            pruned: 0,
            actions: entries
                .iter()
                .map(|entry| ReflogExpireAction {
                    refname: refname.to_string(),
                    entry: entry.clone(),
                    action: ReflogExpireActionKind::Keep,
                })
                .collect(),
        });
    }

    // Old entries are judged against the commits reachable from the ref's own tip (every ref
    // for HEAD), as Git does; a tip that does not name a commit expires them all.
    let tips = if !may_drop_unreachable || expire_unreachable <= expire_total {
        None
    } else if is_head_ref(refname) {
        Some(head_reflog_tips(repo, cache.promised(repo)?))
    } else {
        let promised = cache.promised(repo)?;
        store
            .resolve(refname)
            .ok()
            .and_then(|tip| peel_tip_to_commit(repo, promised, tip))
            .map(|tip| vec![tip])
    };
    let mut reachable = tips.map(|tips| ReachableCommits::new(repo, cache, tips, expire_total));
    let snapshot = entries;
    let mut drop_flags = Vec::with_capacity(snapshot.len());
    let mut pruned = 0usize;
    let mut actions = Vec::with_capacity(snapshot.len());

    for entry in &snapshot {
        let drop = should_drop_reflog_entry(
            repo,
            entry,
            expire_total,
            expire_unreachable,
            reachable.as_mut(),
            stale_fix.as_ref(),
        );
        drop_flags.push(drop);
        let action = if drop {
            pruned += 1;
            if params.dry_run {
                ReflogExpireActionKind::WouldPrune
            } else {
                ReflogExpireActionKind::Prune
            }
        } else {
            ReflogExpireActionKind::Keep
        };
        actions.push(ReflogExpireAction {
            refname: refname.to_string(),
            entry: entry.clone(),
            action,
        });
    }

    if !params.dry_run && pruned > 0 {
        rewrite_reflog_from_snapshot(store, refname, &snapshot, &drop_flags)?;
        // Git applies `core.sharedRepository` to the rewritten log file.
        if let Some(files) = store.as_files() {
            adjust_reflog_shared_perm(repo.git_dir(), &files.reflog_path(refname));
        }
    }
    Ok(ReflogExpireResult { pruned, actions })
}

const REFLOG_REWRITE_RETRIES: u32 = 128;

fn rewrite_reflog_from_snapshot(
    store: &dyn crate::refs::store::RefStore,
    refname: &str,
    snapshot: &[ReflogEntry],
    drop_flags: &[bool],
) -> Result<()> {
    debug_assert_eq!(snapshot.len(), drop_flags.len());
    for attempt in 0..REFLOG_REWRITE_RETRIES {
        let snap = snapshot.to_vec();
        let drops = drop_flags.to_vec();
        let name = refname.to_owned();
        match store.rewrite_reflog(refname, &mut move |current| {
            if current.len() < snap.len() || current[..snap.len()] != snap[..] {
                return Err(Error::RefStore(RefStoreError::ReflogRewriteStale {
                    name: name.clone(),
                }));
            }
            let mut kept = Vec::new();
            for (i, entry) in current.into_iter().enumerate() {
                if i < drops.len() {
                    if !drops[i] {
                        kept.push(entry);
                    }
                } else {
                    kept.push(entry);
                }
            }
            Ok(kept)
        }) {
            Ok(()) => return Ok(()),
            Err(Error::RefStore(RefStoreError::ReflogRewriteStale { .. }))
                if attempt + 1 < REFLOG_REWRITE_RETRIES =>
            {
                continue;
            }
            Err(err) => return Err(err),
        }
    }
    Err(Error::RefStore(RefStoreError::Corrupt(format!(
        "reflog '{refname}' kept changing during rewrite"
    ))))
}

/// Per-ref `gc.<pattern>.reflogExpire*` rules plus global `gc.reflogExpire` / `gc.reflogExpireUnreachable`.
#[derive(Debug, Clone)]
pub struct GcReflogExpireConfig {
    /// `patterns` field.
    pub patterns: Vec<GcReflogPattern>,
    /// `global_total` field.
    pub global_total: Option<i64>,
    /// `global_unreachable` field.
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
    if let Ok(oid) = refs::resolve_ref_in(repo, "HEAD") {
        seeds.push(oid);
    }
    if let Ok(refs) = refs::list_refs_for_repository(repo, "refs/") {
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
        let Ok(Some(obj)) = repo.odb().try_read_once(&oid) else {
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
mod reverse_scan_tests {
    use std::fs;
    use std::io::Write;

    use super::{reflog_path, ReverseReflogLineReader, REFLOG_REVERSE_READ_CHUNK};
    use crate::objects::ObjectId;

    fn sample_line(n: usize) -> String {
        let oid = ObjectId::zero();
        format!(
            "{oid} {oid} T <t@example.com> {ts} +0000\tcommit: line{n}\n",
            ts = 1_700_000_000 + n as i64
        )
    }

    #[test]
    fn reverse_reader_returns_newest_first() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join(".git");
        let path = reflog_path(&git_dir, "refs/heads/main");
        fs::create_dir_all(path.parent().expect("reflog parent")).expect("mkdir");
        let mut f = fs::File::create(&path).expect("create");
        write!(f, "{}", sample_line(1)).expect("w");
        write!(f, "{}", sample_line(2)).expect("w");
        let mut reader = ReverseReflogLineReader::open(&path)
            .expect("open")
            .expect("nonempty");
        let first = reader.next_nonempty_line().expect("read").expect("line");
        assert!(first.ends_with(b"line2"));
        let second = reader.next_nonempty_line().expect("read").expect("line");
        assert!(second.ends_with(b"line1"));
        assert!(reader.next_nonempty_line().expect("read").is_none());
    }

    #[test]
    fn reverse_reader_spans_chunk_boundaries() {
        let dir = tempfile::tempdir().expect("tempdir");
        let git_dir = dir.path().join(".git");
        fs::create_dir_all(git_dir.join("logs")).expect("mkdir");
        let path = reflog_path(&git_dir, "HEAD");
        let mut f = fs::File::create(&path).expect("create");
        let pad = "x".repeat(REFLOG_REVERSE_READ_CHUNK + 128);
        write!(f, "{pad}\n").expect("pad");
        write!(f, "{}", sample_line(99)).expect("newest");
        let mut reader = ReverseReflogLineReader::open(&path)
            .expect("open")
            .expect("nonempty");
        let line = reader.next_nonempty_line().expect("read").expect("line");
        assert!(line.ends_with(b"line99"));
    }
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
    use super::{parse_reflog_expire_spec, parse_reflog_line};

    #[test]
    fn gc_main_suffix_pattern_does_not_match_main_ref() {
        let now = 1_700_000_000_i64;
        let (tmp, _repo) = seed_repo();
        let git_dir = tmp.path().join(".git");
        std::fs::write(
            git_dir.join("config"),
            "[core]\nrepositoryformatversion = 0\n[gc \"refs/heads/main*suffix\"]\n\treflogExpire = now\n",
        )
        .expect("write config");
        let cfg = ConfigSet::load(&Environment::empty(), Some(&git_dir), true).expect("cfg");
        let gc = load_gc_reflog_expire_config(&cfg, now);
        assert_eq!(gc.patterns.len(), 1);
        assert_eq!(gc.patterns[0].pattern, "refs/heads/main*suffix");
        let (total, _) = super::resolve_expire_for_ref(
            "refs/heads/main",
            None,
            None,
            &gc.patterns,
            super::default_expire_total(now),
            super::default_expire_unreachable(now),
        );
        assert_ne!(
            total,
            i64::MAX,
            "refs/heads/main must not pick up main*suffix reflogExpire=now"
        );
        assert_eq!(total, super::default_expire_total(now));
    }

    #[test]
    fn gc_reflog_expiry_uses_git_approxidate_threshold() {
        let now = 1_700_000_000_i64;
        assert_eq!(parse_reflog_expire_spec("never", now).expect("never"), 0);
        assert_eq!(parse_reflog_expire_spec("now", now).expect("now"), i64::MAX);
        // Git `match_digit`: bare integers >= 100_000_000 are Unix timestamps.
        assert_eq!(
            parse_reflog_expire_spec("100000000", now).expect("100M"),
            100_000_000
        );
        // 86_401..99_999_999 are not literal epoch seconds (e.g. 90000 → approxidate, not 90000).
        assert_ne!(
            parse_reflog_expire_spec("90000", now).expect("90000"),
            90_000
        );
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
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
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
        let git_dir = &repo.git_dir();
        let oid = crate::refs::test_commit(git_dir, "aaaaaaaa");
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
        let git_dir = &repo.git_dir();
        let refname = "refs/heads/main";
        let o1 = ObjectId::from_hex("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb").expect("o1");
        let o2 = crate::refs::test_commit(git_dir, "cccccccc");
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
        let git_dir = &repo.git_dir();
        let now = 1_700_000_000_i64;
        let refname = "refs/stash";
        let oid = crate::refs::test_commit(git_dir, "dddddddd");
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
        let git_dir = &repo.git_dir();
        let oid = crate::refs::test_commit(git_dir, "eeeeeeee");
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
        let git_dir = &repo.git_dir();
        let oid = crate::refs::test_commit(git_dir, "12121212");
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
        let git_dir = &repo.git_dir();
        let branch = "refs/heads/main";
        let oid = crate::refs::test_commit(git_dir, "13131313");
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
        let git_dir = &repo.git_dir();
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
    fn reflog_entry_action_prefixes() {
        let cases = [
            ("commit: init", Some("commit")),
            ("commit (initial): init", Some("commit (initial)")),
            ("commit (amend): tweak", Some("commit (amend)")),
            ("checkout: moving from main to topic", Some("checkout")),
            ("reset: moving to HEAD", Some("reset")),
            (
                "rebase (finish): refs/heads/main onto abc",
                Some("rebase (finish)"),
            ),
            ("branch: Created from HEAD", Some("branch")),
            ("branch: Reset to origin/main", Some("branch")),
            ("merge topic: Merge branch 'topic'", Some("merge topic")),
            ("pull: Fast-forward", Some("pull")),
            ("pull: rebase onto origin/main", Some("pull")),
            ("cherry-pick: subject", Some("cherry-pick")),
            ("revert: subject", Some("revert")),
            ("history undo", None),
            ("rewrite", None),
            ("prune-me", None),
        ];
        for (message, want) in cases {
            let entry = super::ReflogEntry {
                old_oid: ObjectId::zero(),
                new_oid: ObjectId::zero(),
                identity: String::new(),
                message: message.into(),
            };
            assert_eq!(entry.action(), want, "message={message:?}");
        }
    }

    #[test]
    fn expire_git_prunes_old_entry_and_reports_action() {
        let (tmp, repo) = seed_repo();
        let git_dir = &repo.git_dir();
        let refname = "refs/heads/main";
        let oid = crate::refs::test_commit(git_dir, "ffffffff");
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

#[cfg(test)]
mod rewrite_concurrency_tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use crate::lockfile::LockWait;
    use crate::objects::ObjectId;
    use crate::refs::prepare_with_lock_wait;
    use crate::refs::store::{
        Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
        ReflogUpdate,
    };

    use super::{
        delete_reflog_entries_on_store, delete_reflog_entries_rechain_on_store, ReflogEntry,
    };

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    fn append(
        store: &dyn RefStore,
        refname: &str,
        oid: ObjectId,
        ts: i64,
    ) -> crate::error::Result<()> {
        let txn = RefTransaction::new().update(RefUpdate {
            name: refname.to_owned(),
            new_value: Some(RawRef::Direct(oid)),
            expected: Expected::Any,
            reflog: Some(ReflogUpdate {
                identity: format!("T <t@example.com> {ts} +0000"),
                message: "x".into(),
                time: time::OffsetDateTime::from_unix_timestamp(ts).expect("ts"),
                old_oid: None,
                new_oid: Some(oid),
            }),
            flags: RefUpdateFlags::default(),
        })?;
        prepare_with_lock_wait(store, txn, LockWait::Indefinitely)?.commit()?;
        Ok(())
    }

    fn read_entries(store: &dyn RefStore, refname: &str) -> Vec<ReflogEntry> {
        let mut out = Vec::new();
        store
            .for_each_reflog_entry(refname, false, &mut |e| {
                out.push(e.clone());
                std::ops::ControlFlow::Continue(())
            })
            .expect("read");
        out
    }

    #[test]
    fn delete_on_store_uses_locked_entries_not_stale_snapshot() {
        let store = Arc::new(MemoryRefStore::new());
        let refname = "refs/heads/r";
        let o1 = oid(1);
        let o2 = oid(2);
        let o3 = oid(3);
        store
            .replace_reflog(
                refname,
                vec![
                    ReflogEntry {
                        old_oid: ObjectId::zero(),
                        new_oid: o1,
                        identity: "T <t@e.com> 100 +0000".into(),
                        message: String::new(),
                    },
                    ReflogEntry {
                        old_oid: o1,
                        new_oid: o2,
                        identity: "T <t@e.com> 200 +0000".into(),
                        message: String::new(),
                    },
                ],
            )
            .expect("seed");
        let barrier = Arc::new(Barrier::new(2));
        let s_delete = store.clone();
        let b_delete = barrier.clone();
        let deleter = thread::spawn(move || {
            b_delete.wait();
            delete_reflog_entries_on_store(s_delete.as_ref(), refname, &[1]).expect("delete");
        });
        let b_append = barrier.clone();
        let s_append = store.clone();
        let appender = thread::spawn(move || {
            b_append.wait();
            append(s_append.as_ref(), refname, o3, 9_000_000_000).expect("append");
        });
        deleter.join().expect("deleter");
        appender.join().expect("appender");
        let entries = read_entries(store.as_ref(), refname);
        assert!(
            entries.iter().any(|e| e.new_oid == o3),
            "concurrent append missing after delete rewrite"
        );
        assert_eq!(entries.len(), 2, "expected oldest removed and append kept");
    }

    #[test]
    fn delete_rechain_on_store_uses_locked_entries_not_stale_snapshot() {
        let store = Arc::new(MemoryRefStore::new());
        let refname = "refs/heads/r";
        let o1 = oid(1);
        let o2 = oid(2);
        let o3 = oid(3);
        store
            .replace_reflog(
                refname,
                vec![
                    ReflogEntry {
                        old_oid: ObjectId::zero(),
                        new_oid: o1,
                        identity: "T <t@e.com> 100 +0000".into(),
                        message: String::new(),
                    },
                    ReflogEntry {
                        old_oid: o1,
                        new_oid: o2,
                        identity: "T <t@e.com> 200 +0000".into(),
                        message: String::new(),
                    },
                ],
            )
            .expect("seed");
        let barrier = Arc::new(Barrier::new(2));
        let s_delete = store.clone();
        let b_delete = barrier.clone();
        let deleter = thread::spawn(move || {
            b_delete.wait();
            delete_reflog_entries_rechain_on_store(s_delete.as_ref(), refname, &[1])
                .expect("rechain delete");
        });
        let b_append = barrier.clone();
        let s_append = store.clone();
        let appender = thread::spawn(move || {
            b_append.wait();
            append(s_append.as_ref(), refname, o3, 9_000_000_000).expect("append");
        });
        deleter.join().expect("deleter");
        appender.join().expect("appender");
        let entries = read_entries(store.as_ref(), refname);
        assert!(entries.iter().any(|e| e.new_oid == o3));
    }
}

#[cfg(test)]
mod completeness_tests {
    use std::collections::HashSet;

    use tempfile::TempDir;

    use crate::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
    use crate::repo::{init_repository, Repository};

    use super::{Linkage, ReachableCommits, ReflogExpiryCache};

    fn seed_repo() -> (TempDir, Repository) {
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(
            tmp.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        (tmp, repo)
    }

    fn tree(repo: &Repository, entries: &[(u32, &str, ObjectId)]) -> ObjectId {
        let entries = entries
            .iter()
            .map(|&(mode, name, oid)| TreeEntry {
                mode,
                name: name.as_bytes().to_vec(),
                oid,
            })
            .collect::<Vec<_>>();
        repo.odb()
            .write(ObjectKind::Tree, &serialize_tree(&entries))
            .expect("write tree")
    }

    fn commit(repo: &Repository, tree: ObjectId, parents: &[ObjectId]) -> ObjectId {
        commit_at(repo, tree, parents, 1)
    }

    fn commit_at(repo: &Repository, tree: ObjectId, parents: &[ObjectId], when: i64) -> ObjectId {
        let mut body = format!("tree {tree}\n");
        for p in parents {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author A <a@example.com> {when} +0000\ncommitter A <a@example.com> {when} +0000\n\nc\n"
        ));
        repo.odb()
            .write(ObjectKind::Commit, body.as_bytes())
            .expect("write commit")
    }

    #[test]
    fn completeness_walk_is_memoised_and_stops_at_missing_objects_and_promises() {
        let (_tmp, repo) = seed_repo();
        let blob = repo.odb().write(ObjectKind::Blob, b"a\n").expect("blob");
        let sub = tree(&repo, &[(0o100644, "b", blob)]);
        let root = tree(&repo, &[(0o100644, "a", blob), (0o040000, "sub", sub)]);
        let base = commit(&repo, root, &[]);
        let left = commit(&repo, root, &[base]);
        let right = commit(&repo, sub, &[base]);
        let merge = commit(&repo, root, &[left, right]);

        let none = HashSet::new();
        let cache = ReflogExpiryCache::default();
        assert!(cache.commit_complete(&repo, &none, ObjectId::zero()));
        let probes = repo.odb().exists_probe_count();
        assert!(cache.commit_complete(&repo, &none, merge));
        assert_eq!(
            repo.odb().exists_probe_count() - probes,
            2,
            "each blob entry of each distinct tree is probed once"
        );
        let probes = repo.odb().exists_probe_count();
        assert!(cache.commit_complete(&repo, &none, left));
        assert!(cache.commit_complete(&repo, &none, merge));
        assert_eq!(
            repo.odb().exists_probe_count(),
            probes,
            "answers are reused"
        );

        let missing = ObjectId::from_hex("1111111111111111111111111111111111111111").expect("oid");
        let broken_tree = tree(&repo, &[(0o100644, "gone", missing)]);
        let broken = commit(&repo, broken_tree, &[merge]);
        let child = commit(&repo, root, &[broken]);
        assert!(!cache.commit_complete(&repo, &none, broken));
        assert!(!cache.commit_complete(&repo, &none, child));
        assert!(!cache.commit_complete(&repo, &none, missing));
        assert!(!cache.commit_complete(&repo, &none, blob), "not a commit");

        let promised = HashSet::from([missing]);
        let fresh = ReflogExpiryCache::default();
        assert!(fresh.commit_complete(&repo, &promised, child));
        assert!(fresh.commit_complete(&repo, &promised, missing));
    }

    #[test]
    fn reachability_walk_stops_at_the_cutoff_until_a_lookup_misses() {
        let (_tmp, repo) = seed_repo();
        let tree = tree(&repo, &[]);
        let mut chain = Vec::new();
        for when in 1..=10 {
            let parents = chain.last().copied().into_iter().collect::<Vec<_>>();
            chain.push(commit_at(&repo, tree, &parents, when));
        }
        let unrelated = commit_at(&repo, tree, &[], 5);

        let cache = ReflogExpiryCache::default();
        let mut reachable = ReachableCommits::new(&repo, &cache, vec![chain[9]], 8);
        assert_eq!(
            reachable.seen_count, 4,
            "commits from time 10 down to the first one older than the cutoff"
        );
        assert!(reachable.contains(&chain[7]));
        assert_eq!(reachable.seen_count, 4, "a hit does not walk further");
        assert!(reachable.contains(&chain[0]));
        assert_eq!(reachable.seen_count, 10, "a miss walks the rest once");
        assert!(!reachable.contains(&unrelated));

        let mut everything = ReachableCommits::new(&repo, &cache, vec![chain[9]], 0);
        assert_eq!(everything.seen_count, 10);
        assert!(!everything.contains(&unrelated));
        assert_eq!(
            cache.commits.borrow().len(),
            10,
            "both walks share one interned commit per commit"
        );

        {
            let mut commits = cache.commits.borrow_mut();
            let tip = commits.intern(chain[9]) as usize;
            let parent = commits.intern(unrelated);
            commits.nodes[tip] = Linkage::Read {
                time: 10,
                parents: Box::new([parent]),
            };
        }
        let mut reused = ReachableCommits::new(&repo, &cache, vec![chain[9]], 0);
        assert!(
            reused.contains(&unrelated),
            "a later walk in the same pass takes linkage from the cache"
        );
        assert_eq!(
            ReachableCommits::new(&repo, &cache, Vec::new(), 0).seen_count,
            0
        );
    }

    #[test]
    fn reachability_walk_counts_shallow_boundaries_as_reachable_without_walking_past_them() {
        let (_tmp, repo) = seed_repo();
        let tree = tree(&repo, &[]);
        let root = commit_at(&repo, tree, &[], 1);
        let boundary = commit_at(&repo, tree, &[root], 2);
        let tip = commit_at(&repo, tree, &[boundary], 3);
        std::fs::write(repo.git_dir().join("shallow"), format!("{boundary}\n")).expect("shallow");

        let cache = ReflogExpiryCache::default();
        let mut reachable = ReachableCommits::new(&repo, &cache, vec![tip], 0);
        assert!(reachable.contains(&boundary));
        assert!(!reachable.contains(&root));
        assert_eq!(reachable.seen_count, 2);
    }
}
