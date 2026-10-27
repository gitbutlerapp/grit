//! Undo log for one checkout pass, so a failed or cancelled checkout leaves the work tree as it
//! found it.
//!
//! Recording is cheap on the success path: created and replaced paths are remembered by name,
//! removed directories by prefix, and only per-file deletions move bytes (into a private spool,
//! bounded memory, cross-device safe). Rollback rebuilds replaced and wholesale-removed paths
//! from the on-disk index, which a checkout writes only as its last step.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::diff::{DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};
use crate::objects::ObjectId;
use crate::repo::Repository;
use crate::worktree::{io_with_path, WorktreePathOperation};
use crate::worktree_rules::WorktreeRules;

use super::{
    materialize_gitlink_worktree, parse_git_mode, path_under_wholesale_directory,
    remove_empty_parent_dirs_batch, remove_uninitialized_gitlink_worktree, write_blob_to_worktree,
    LeadingDirCache,
};

static SPOOL_NAME_SEQ: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SpooledPathKind {
    Regular,
    Symlink,
}

#[derive(Debug)]
struct SpooledCheckoutDelete {
    rel: String,
    stash: PathBuf,
    kind: SpooledPathKind,
}

/// Worktree files removed during checkout, moved into a spool so a later failure can restore them.
struct CheckoutDeleteSpool {
    spool: tempfile::TempDir,
    entries: Vec<SpooledCheckoutDelete>,
    /// A restore failed: keep the spool directory, which holds the only copy of those files.
    keep: bool,
}

impl CheckoutDeleteSpool {
    /// Create a private spool directory (respects `TMPDIR`).
    fn new() -> Result<Self> {
        Ok(Self {
            spool: tempfile::Builder::new()
                .prefix("grit-checkout-del-")
                .tempdir()
                .map_err(Error::Io)?,
            entries: Vec::new(),
            keep: false,
        })
    }

    /// Move `rel_path` out of the worktree into the spool, or no-op when absent.
    fn spool_remove(&mut self, work_tree: &Path, rel_path: &str) -> Result<()> {
        let abs = crate::repo_relpath::worktree_abs(work_tree, rel_path);
        let meta = match fs::symlink_metadata(&abs) {
            Ok(m) => m,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return io_with_path(rel_path, WorktreePathOperation::Remove, Err(e));
            }
        };
        let kind = if meta.file_type().is_symlink() {
            SpooledPathKind::Symlink
        } else if meta.is_file() {
            SpooledPathKind::Regular
        } else {
            return Ok(());
        };
        let stash = self.spool.path().join(spool_entry_name(rel_path));
        if let Err(e) = move_worktree_entry_to_spool(&abs, &stash, kind) {
            return io_with_path(rel_path, WorktreePathOperation::Remove, Err(e));
        }
        self.entries.push(SpooledCheckoutDelete {
            rel: rel_path.to_owned(),
            stash,
            kind,
        });
        Ok(())
    }

    /// Put spooled paths back (newest deletions first); returns the paths that could not be.
    fn restore_all(&mut self, work_tree: &Path) -> Vec<String> {
        let mut failed = Vec::new();
        for entry in self.entries.drain(..).rev() {
            if restore_spooled_entry(work_tree, &entry).is_err() {
                failed.push(entry.rel);
            }
        }
        self.keep |= !failed.is_empty();
        failed
    }
}

impl Drop for CheckoutDeleteSpool {
    fn drop(&mut self) {
        if self.keep {
            self.spool.disable_cleanup(true);
        }
    }
}

/// Shared handle to a journal's delete spool, for parallel checkout deletes.
pub(crate) struct SharedCheckoutDeleteRollback(Arc<Mutex<CheckoutDeleteSpool>>);

impl SharedCheckoutDeleteRollback {
    pub(crate) fn clone_handle(&self) -> Self {
        Self(Arc::clone(&self.0))
    }

    /// Move `rel_path` into the spool so rollback can restore it; no-op when absent.
    pub(crate) fn spool_remove(&self, work_tree: &Path, rel_path: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spool_remove(work_tree, rel_path)
    }
}

/// Repository-relative paths packed into one buffer: a populated checkout records every path
/// it creates, so avoid a separate allocation per path.
#[derive(Default)]
struct PathList {
    bytes: String,
    ends: Vec<usize>,
}

impl PathList {
    fn push(&mut self, path: &str) {
        self.bytes.push_str(path);
        self.ends.push(self.bytes.len());
    }

    fn len(&self) -> usize {
        self.ends.len()
    }

    /// Paths, newest first.
    fn iter_rev(&self) -> impl Iterator<Item = &str> {
        self.ends.iter().enumerate().rev().map(|(i, &end)| {
            let start = i.checked_sub(1).map_or(0, |prev| self.ends[prev]);
            &self.bytes[start..end]
        })
    }
}

/// A path whose previous content a checkout overwrote.
struct ReplacedPath {
    path: String,
    oid: ObjectId,
    mode: u32,
}

/// Everything one checkout pass changed in the work tree, until the pass commits.
///
/// The pass commits when its index reaches disk ([`Self::commit`]); from then on the new tree is
/// the repository state and nothing is undone. [`Self::finish`] rolls an uncommitted pass back.
pub(crate) struct CheckoutJournal {
    work_tree: PathBuf,
    spool: Arc<Mutex<CheckoutDeleteSpool>>,
    /// Paths that did not exist before the checkout wrote them.
    created: PathList,
    /// Paths whose tracked content the checkout overwrote.
    replaced: Vec<ReplacedPath>,
    /// Wholesale-removed directories (whose index entries rollback writes back).
    removed_dirs: BTreeSet<String>,
    /// Uninitialized gitlink directories the checkout removed.
    removed_gitlinks: Vec<String>,
    /// Paths never written whose leading directories the checkout may already have created.
    unwritten: Vec<String>,
    committed: bool,
}

impl CheckoutJournal {
    /// Start an empty journal for a checkout of `work_tree`.
    pub(crate) fn new(work_tree: &Path) -> Result<Self> {
        Ok(Self {
            work_tree: work_tree.to_path_buf(),
            spool: Arc::new(Mutex::new(CheckoutDeleteSpool::new()?)),
            created: PathList::default(),
            replaced: Vec::new(),
            removed_dirs: BTreeSet::new(),
            removed_gitlinks: Vec::new(),
            unwritten: Vec::new(),
            committed: false,
        })
    }

    /// Handle for spooling deletions from worker threads.
    pub(crate) fn shared(&self) -> SharedCheckoutDeleteRollback {
        SharedCheckoutDeleteRollback(Arc::clone(&self.spool))
    }

    /// Move `rel_path` into the spool so rollback can restore it; no-op when absent.
    pub(crate) fn spool_remove(&self, rel_path: &str) -> Result<()> {
        self.spool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spool_remove(&self.work_tree, rel_path)
    }

    /// Record that the checkout is about to write `change`'s destination path.
    pub(crate) fn record_write(&mut self, change: &DiffEntry) {
        let Some(path) = change.new_path.as_deref() else {
            return;
        };
        if change.status == DiffStatus::Added {
            self.created.push(path);
        } else {
            self.replaced.push(ReplacedPath {
                path: path.to_owned(),
                oid: change.old_oid,
                mode: parse_git_mode(&change.old_mode),
            });
        }
    }

    /// Record that the checkout prepared the leading directories of `rel_path` but never wrote
    /// it, so rollback removes those directories again when they are empty.
    pub(crate) fn record_unwritten(&mut self, rel_path: &str) {
        self.unwritten.push(rel_path.to_owned());
    }

    /// Record that the checkout is about to create `rel_path`, which does not exist yet.
    pub(crate) fn record_created(&mut self, rel_path: &str) {
        self.created.push(rel_path);
    }

    /// Record that the checkout is about to remove the whole tracked directory `dir`.
    pub(crate) fn record_removed_dir(&mut self, dir: &str) {
        self.removed_dirs.insert(dir.to_owned());
    }

    /// Remove the gitlink worktree at `rel_path` unless it is an initialized submodule,
    /// remembering it for rollback when something was there.
    pub(crate) fn remove_uninitialized_gitlink(&mut self, rel_path: &str) -> PathBuf {
        let abs = crate::repo_relpath::worktree_abs(&self.work_tree, rel_path);
        if fs::symlink_metadata(&abs).is_ok() {
            self.removed_gitlinks.push(rel_path.to_owned());
            remove_uninitialized_gitlink_worktree(&abs);
        }
        abs
    }

    /// Before overwriting `rel_path`, keep its current bytes for rollback: spool an existing
    /// file or symlink, or remember that the path is new.
    pub(crate) fn preserve_before_write(&mut self, rel_path: &str) -> Result<()> {
        let abs = crate::repo_relpath::worktree_abs(&self.work_tree, rel_path);
        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => Ok(()),
            Ok(_) => self.spool_remove(rel_path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.record_created(rel_path);
                Ok(())
            }
            Err(e) => io_with_path(rel_path, WorktreePathOperation::Replace, Err(e)),
        }
    }

    /// The checkout's index is on disk: keep every change and drop the spool.
    pub(crate) fn commit(&mut self) {
        self.committed = true;
        self.created = PathList::default();
        self.replaced = Vec::new();
        self.removed_dirs = BTreeSet::new();
        self.removed_gitlinks = Vec::new();
        self.unwritten = Vec::new();
        self.spool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entries
            .clear();
    }

    /// End the pass with `result`: commit on success, roll back an uncommitted failure.
    ///
    /// # Errors
    ///
    /// Returns the checkout's own error after a complete rollback, or
    /// [`Error::CheckoutRollbackIncomplete`] wrapping it with the paths that could not be put
    /// back.
    pub(crate) fn finish<T>(mut self, repo: &Repository, result: Result<T>) -> Result<T> {
        let err = match result {
            Ok(value) => {
                self.commit();
                return Ok(value);
            }
            Err(err) if self.committed => return Err(err),
            Err(err) => err,
        };
        let mut unrestored = self.rollback(repo);
        unrestored.sort();
        unrestored.dedup();
        if unrestored.is_empty() {
            return Err(err);
        }
        Err(Error::CheckoutRollbackIncomplete {
            source: Box::new(err),
            unrestored,
        })
    }

    /// Undo every recorded change; returns the paths left in a different state.
    fn rollback(&mut self, repo: &Repository) -> Vec<String> {
        let mut unrestored = Vec::new();
        let created = std::mem::take(&mut self.created);
        let mut removed = Vec::with_capacity(created.len());
        for rel in created.iter_rev() {
            let abs = crate::repo_relpath::worktree_abs(&self.work_tree, rel);
            match remove_created_path(&abs) {
                Ok(()) => removed.push(abs),
                Err(_) => unrestored.push(rel.to_owned()),
            }
        }
        removed.extend(
            self.unwritten
                .drain(..)
                .map(|rel| crate::repo_relpath::worktree_abs(&self.work_tree, &rel)),
        );
        remove_empty_parent_dirs_batch(&self.work_tree, &removed);

        unrestored.extend(
            self.spool
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .restore_all(&self.work_tree),
        );

        let replaced = std::mem::take(&mut self.replaced);
        let removed_gitlinks = std::mem::take(&mut self.removed_gitlinks);
        let removed_dirs = std::mem::take(&mut self.removed_dirs);
        if replaced.is_empty() && removed_gitlinks.is_empty() && removed_dirs.is_empty() {
            return unrestored;
        }
        let mut restore = RestoreFromIndex::new(repo, &self.work_tree);
        for entry in replaced.iter().rev() {
            if !restore.path(&entry.path, Some((entry.oid, entry.mode))) {
                unrestored.push(entry.path.clone());
            }
        }
        for rel in &removed_gitlinks {
            if !restore.path(rel, Some((ObjectId::zero(), MODE_GITLINK))) {
                unrestored.push(rel.clone());
            }
        }
        if !removed_dirs.is_empty() {
            unrestored.extend(restore.directories(&removed_dirs));
        }
        unrestored
    }
}

impl Drop for CheckoutJournal {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self
                .spool
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .restore_all(&self.work_tree);
        }
    }
}

/// Writes paths back as the on-disk index (the pre-checkout state) records them.
struct RestoreFromIndex<'a> {
    repo: &'a Repository,
    work_tree: &'a Path,
    index: Option<Index>,
    rules: Option<WorktreeRules>,
    dir_cache: LeadingDirCache,
}

impl<'a> RestoreFromIndex<'a> {
    fn new(repo: &'a Repository, work_tree: &'a Path) -> Self {
        let index = repo.load_index().ok();
        let rules = index.as_ref().and_then(|index| {
            let config = repo.config().ok()?;
            WorktreeRules::from_parts_index_attributes_only(repo, index, config).ok()
        });
        Self {
            repo,
            work_tree,
            index,
            rules,
            dir_cache: LeadingDirCache::new(),
        }
    }

    /// Restore `rel` from its stage-0 index entry, else from `fallback` (`oid`, `mode`).
    /// Returns `false` when the path could not be written.
    fn path(&mut self, rel: &str, fallback: Option<(ObjectId, u32)>) -> bool {
        let key = crate::repo_relpath::storage_bytes(rel);
        let tracked = self
            .index
            .as_ref()
            .and_then(|index| index.get(&key, 0))
            .filter(|entry| !entry.skip_worktree())
            .map(|entry| (entry.oid, entry.mode));
        match tracked.or(fallback) {
            Some((oid, mode)) => self.write(rel, oid, mode),
            None => true,
        }
    }

    /// Restore every stage-0 index entry under `dirs`; returns the paths that failed (or the
    /// directories themselves when the index could not be read).
    fn directories(&mut self, dirs: &BTreeSet<String>) -> Vec<String> {
        let Some(index) = self.index.take() else {
            return dirs.iter().cloned().collect();
        };
        let mut failed = Vec::new();
        for entry in &index.entries {
            if entry.stage() != 0
                || entry.skip_worktree()
                || entry.is_sparse_directory_placeholder()
            {
                continue;
            }
            let rel = crate::repo_relpath::storage_string(&entry.path);
            if path_under_wholesale_directory(&rel, dirs)
                && !self.write(&rel, entry.oid, entry.mode)
            {
                failed.push(rel);
            }
        }
        self.index = Some(index);
        failed
    }

    fn write(&mut self, rel: &str, oid: ObjectId, mode: u32) -> bool {
        if mode == MODE_GITLINK {
            return materialize_gitlink_worktree(self.work_tree, rel, &mut self.dir_cache).is_ok();
        }
        let Some(rules) = &self.rules else {
            return false;
        };
        matches!(
            write_blob_to_worktree(
                self.repo,
                rules,
                self.work_tree,
                rel,
                &oid,
                mode,
                &mut self.dir_cache
            ),
            Ok(Some(_))
        )
    }
}

/// Remove a path the checkout created: a file, a symlink or an empty gitlink directory.
fn remove_created_path(abs: &Path) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(abs) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.is_dir() {
        fs::remove_dir(abs)
    } else {
        fs::remove_file(abs)
    }
}

fn spool_entry_name(rel: &str) -> String {
    let seq = SPOOL_NAME_SEQ.fetch_add(1, Ordering::Relaxed);
    let safe: String = rel
        .chars()
        .map(|c| if c == '/' || c == '\\' { '_' } else { c })
        .collect();
    format!("{}_{}_{safe}", std::process::id(), seq)
}

fn move_worktree_entry_to_spool(
    abs: &Path,
    stash: &Path,
    kind: SpooledPathKind,
) -> std::io::Result<()> {
    if let Some(parent) = stash.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(abs, stash) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            cross_device_move_to_spool(abs, stash, kind)?;
            fs::remove_file(abs)?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn cross_device_move_to_spool(
    abs: &Path,
    stash: &Path,
    kind: SpooledPathKind,
) -> std::io::Result<()> {
    match kind {
        SpooledPathKind::Symlink => {
            let target = fs::read_link(abs)?;
            create_symlink_at(&target, stash)?;
        }
        SpooledPathKind::Regular => {
            fs::copy(abs, stash)?;
        }
    }
    Ok(())
}

fn restore_spooled_entry(work_tree: &Path, entry: &SpooledCheckoutDelete) -> std::io::Result<()> {
    let abs = crate::repo_relpath::worktree_abs(work_tree, &entry.rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }
    if abs.exists() || abs.symlink_metadata().is_ok() {
        if abs.is_dir() {
            fs::remove_dir_all(&abs)?;
        } else {
            let _ = fs::remove_file(&abs);
        }
    }
    match fs::rename(&entry.stash, &abs) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            cross_device_restore_from_spool(&entry.stash, &abs, entry.kind)?;
            let _ = fs::remove_file(&entry.stash);
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn cross_device_restore_from_spool(
    stash: &Path,
    abs: &Path,
    kind: SpooledPathKind,
) -> std::io::Result<()> {
    match kind {
        SpooledPathKind::Symlink => {
            let target = fs::read_link(stash)?;
            create_symlink_at(&target, abs)?;
        }
        SpooledPathKind::Regular => {
            fs::copy(stash, abs)?;
        }
    }
    Ok(())
}

fn create_symlink_at(target: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        symlink(target, link)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::{symlink_dir, symlink_file};
        if target.is_dir() {
            symlink_dir(target, link)
        } else {
            symlink_file(target, link)
        }
    }
}

#[cfg(test)]
mod path_list_tests {
    use super::PathList;

    #[test]
    fn paths_come_back_newest_first_and_intact() {
        let mut list = PathList::default();
        for path in ["a", "dir/b.txt", "dir/sub/ünï.txt", "c"] {
            list.push(path);
        }
        assert_eq!(list.len(), 4);
        assert_eq!(
            list.iter_rev().collect::<Vec<_>>(),
            ["c", "dir/sub/ünï.txt", "dir/b.txt", "a"]
        );
        assert_eq!(PathList::default().iter_rev().count(), 0);
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn cross_device_spool_roundtrip_preserves_symlink() {
        let wt = tempfile::tempdir().expect("worktree");
        let spool_root = PathBuf::from("/dev/shm");
        if !spool_root.is_dir() {
            return;
        }
        let spool = tempfile::Builder::new()
            .prefix("grit-checkout-del-test-")
            .tempdir_in(&spool_root)
            .expect("spool in shm");
        symlink("target-bytes", wt.path().join("a-link")).expect("symlink");
        let stash = spool.path().join("entry");
        move_worktree_entry_to_spool(&wt.path().join("a-link"), &stash, SpooledPathKind::Symlink)
            .expect("spool move");
        assert!(!wt.path().join("a-link").exists());
        restore_spooled_entry(
            wt.path(),
            &SpooledCheckoutDelete {
                rel: "a-link".into(),
                stash,
                kind: SpooledPathKind::Symlink,
            },
        )
        .expect("restore");
        assert!(wt
            .path()
            .join("a-link")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        assert_eq!(
            fs::read_link(wt.path().join("a-link")).unwrap(),
            PathBuf::from("target-bytes")
        );
    }
}
