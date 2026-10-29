//! Undo log for one checkout pass, so a failed or cancelled checkout leaves the work tree as it
//! found it.
//!
//! Recording is cheap on the success path: created and replaced paths are remembered by name,
//! removed directories by prefix, and only per-file deletions move bytes (into a private spool,
//! bounded memory, cross-device safe). Rollback rebuilds replaced and wholesale-removed paths
//! from the on-disk index, which a checkout writes only as its last step. That is right for
//! clean paths; callers that may overwrite dirty files (hard reset) spool those first, and
//! spooled bytes win over the index.

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use super::{
    materialize_gitlink_worktree, path_under_wholesale_directory, remove_created_checkout_paths,
    remove_empty_parent_dirs_batch, remove_uninitialized_gitlink_worktree,
    restore_checkout_rollback_paths, CheckoutRollbackRestore, LeadingDirCache,
};
use crate::diff::{DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};
use crate::objects::ObjectId;
use crate::progress::NullProgress;
use crate::repo::Repository;
use crate::worktree::{io_with_path, WorktreePathOperation};

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

/// A work-tree directory moved aside so a file could be written at the same path (untracked
/// contents included).
#[derive(Debug)]
struct SpooledDirectoryReplace {
    rel: String,
    stash: PathBuf,
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

    /// Move the directory at `rel_path` (and everything under it) into the spool.
    fn spool_directory_tree(&mut self, work_tree: &Path, rel_path: &str) -> Result<PathBuf> {
        let abs = crate::repo_relpath::worktree_abs(work_tree, rel_path);
        let stash = self
            .spool
            .path()
            .join(spool_entry_name(&format!("dir/{rel_path}")));
        move_directory_tree(&abs, &stash).map_err(Error::Io)?;
        Ok(stash)
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
    /// Directories removed so a regular file or symlink could be written at the same path.
    spooled_directories: Vec<SpooledDirectoryReplace>,
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
            spooled_directories: Vec::new(),
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
                mode: change.old_mode.bits(),
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
    /// file or symlink, move aside an existing directory (including untracked contents), or
    /// remember that the path is new.
    pub(crate) fn preserve_before_write(&mut self, rel_path: &str) -> Result<()> {
        let abs = crate::repo_relpath::worktree_abs(&self.work_tree, rel_path);
        match fs::symlink_metadata(&abs) {
            Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
                self.spool_directory_at_path(rel_path)
            }
            Ok(_) => self.spool_remove(rel_path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                self.record_created(rel_path);
                Ok(())
            }
            Err(e) => io_with_path(rel_path, WorktreePathOperation::Replace, Err(e)),
        }
    }

    /// Move the directory at `rel_path` into the delete spool so rollback can put it back.
    fn spool_directory_at_path(&mut self, rel_path: &str) -> Result<()> {
        let stash = self
            .spool
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spool_directory_tree(&self.work_tree, rel_path)?;
        self.spooled_directories.push(SpooledDirectoryReplace {
            rel: rel_path.to_owned(),
            stash,
        });
        Ok(())
    }

    /// The checkout's index is on disk: keep every change and drop the spool.
    pub(crate) fn commit(&mut self) {
        self.committed = true;
        self.created = PathList::default();
        self.replaced = Vec::new();
        self.removed_dirs = BTreeSet::new();
        self.removed_gitlinks = Vec::new();
        self.unwritten = Vec::new();
        for entry in self.spooled_directories.drain(..) {
            let _ = fs::remove_dir_all(&entry.stash);
        }
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
        let created_paths: Vec<String> = created.iter_rev().map(str::to_owned).collect();
        let (mut removed, create_failed) =
            remove_created_checkout_paths(repo, &self.work_tree, &created_paths);
        unrestored.extend(create_failed);
        removed.extend(
            self.unwritten
                .drain(..)
                .map(|rel| crate::repo_relpath::worktree_abs(&self.work_tree, &rel)),
        );
        remove_empty_parent_dirs_batch(&self.work_tree, &removed);

        // Spooled bytes are what the path held before the checkout; the index must not
        // overwrite them again below.
        let spooled: HashSet<String> = {
            let mut spool = self.spool.lock().unwrap_or_else(|e| e.into_inner());
            let spooled = spool
                .entries
                .iter()
                .map(|entry| entry.rel.clone())
                .collect();
            unrestored.extend(spool.restore_all(&self.work_tree));
            spooled
        };

        for entry in self.spooled_directories.drain(..).rev() {
            if restore_spooled_directory(&self.work_tree, &entry).is_err() {
                unrestored.push(entry.rel);
            }
        }

        let replaced = std::mem::take(&mut self.replaced);
        let removed_gitlinks = std::mem::take(&mut self.removed_gitlinks);
        let removed_dirs = std::mem::take(&mut self.removed_dirs);
        if replaced.is_empty() && removed_gitlinks.is_empty() && removed_dirs.is_empty() {
            return unrestored;
        }

        let index = repo.load_index().ok();
        let mut restore_targets: Vec<CheckoutRollbackRestore> = Vec::new();
        let mut restore_paths = HashSet::new();
        let mut dir_cache = LeadingDirCache::new();
        for entry in replaced.iter().rev() {
            if spooled.contains(&entry.path) {
                continue;
            }
            let (oid, mode) =
                index_entry_or_fallback(index.as_ref(), &entry.path, entry.oid, entry.mode);
            if mode == MODE_GITLINK {
                if materialize_gitlink_worktree(&self.work_tree, &entry.path, &mut dir_cache)
                    .is_err()
                {
                    unrestored.push(entry.path.clone());
                }
                continue;
            }
            if restore_paths.insert(entry.path.clone()) {
                restore_targets.push(CheckoutRollbackRestore {
                    path: entry.path.clone(),
                    oid,
                    mode,
                });
            }
        }
        if let Some(index) = index.as_ref() {
            for entry in &index.entries {
                if entry.stage() != 0
                    || entry.skip_worktree()
                    || entry.is_sparse_directory_placeholder()
                {
                    continue;
                }
                let rel = crate::repo_relpath::storage_string(&entry.path);
                if !path_under_wholesale_directory(&rel, &removed_dirs) || spooled.contains(&rel) {
                    continue;
                }
                if entry.mode == MODE_GITLINK {
                    if materialize_gitlink_worktree(&self.work_tree, &rel, &mut dir_cache).is_err()
                    {
                        unrestored.push(rel);
                    }
                    continue;
                }
                if restore_paths.insert(rel.clone()) {
                    restore_targets.push(CheckoutRollbackRestore {
                        path: rel,
                        oid: entry.oid,
                        mode: entry.mode,
                    });
                }
            }
        } else if !removed_dirs.is_empty() {
            unrestored.extend(removed_dirs.iter().cloned());
        }
        for rel in &removed_gitlinks {
            if materialize_gitlink_worktree(&self.work_tree, rel, &mut dir_cache).is_err() {
                unrestored.push(rel.clone());
            }
        }

        let mut progress = NullProgress;
        unrestored.extend(restore_checkout_rollback_paths(
            repo,
            &self.work_tree,
            &restore_targets,
            &mut progress,
        ));
        unrestored
    }
}

fn index_entry_or_fallback(
    index: Option<&Index>,
    rel: &str,
    fallback_oid: ObjectId,
    fallback_mode: u32,
) -> (ObjectId, u32) {
    let key = crate::repo_relpath::storage_bytes(rel);
    index
        .and_then(|index| index.get(&key, 0))
        .filter(|entry| !entry.skip_worktree())
        .map(|entry| (entry.oid, entry.mode))
        .unwrap_or((fallback_oid, fallback_mode))
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

/// Longest file-name tail kept in a spool entry name, so a kept spool stays recognizable while
/// the whole name fits one path component (255 bytes on common filesystems).
const SPOOL_NAME_TAIL_BYTES: usize = 64;

/// A spool file name unique within the spool. The repository path lives on the entry itself;
/// the name keeps at most a short tail of the file name, never the directories, so deep paths
/// cannot exceed the component length limit.
fn spool_entry_name(rel: &str) -> String {
    let seq = SPOOL_NAME_SEQ.fetch_add(1, Ordering::Relaxed);
    let file_name = rel.rsplit(['/', '\\']).next().unwrap_or_default();
    let mut start = file_name.len().saturating_sub(SPOOL_NAME_TAIL_BYTES);
    while !file_name.is_char_boundary(start) {
        start += 1;
    }
    format!("{}_{seq}_{}", std::process::id(), &file_name[start..])
}

fn move_directory_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    match fs::rename(from, to) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::CrossesDevices => {
            copy_directory_tree(from, to)?;
            fs::remove_dir_all(from)?;
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn copy_directory_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    fs::create_dir_all(to)?;
    for entry in fs::read_dir(from)? {
        let entry = entry?;
        let src = entry.path();
        let dest = to.join(entry.file_name());
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::symlink;
                symlink(fs::read_link(&src)?, &dest)?;
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::{symlink_dir, symlink_file};
                let target = fs::read_link(&src)?;
                if target.is_dir() {
                    symlink_dir(&target, &dest)?;
                } else {
                    symlink_file(&target, &dest)?;
                }
            }
        } else if file_type.is_dir() {
            copy_directory_tree(&src, &dest)?;
        } else {
            fs::copy(&src, &dest)?;
        }
    }
    Ok(())
}

fn remove_path_for_replace(abs: &Path) -> std::io::Result<()> {
    let meta = match fs::symlink_metadata(abs) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e),
    };
    if meta.file_type().is_symlink() {
        fs::remove_file(abs)
    } else if meta.is_dir() {
        fs::remove_dir_all(abs)
    } else {
        fs::remove_file(abs)
    }
}

fn restore_spooled_directory(
    work_tree: &Path,
    entry: &SpooledDirectoryReplace,
) -> std::io::Result<()> {
    let abs = crate::repo_relpath::worktree_abs(work_tree, &entry.rel);
    remove_path_for_replace(&abs)?;
    move_directory_tree(&entry.stash, &abs)?;
    Ok(())
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

pub(crate) fn create_symlink_at(target: &Path, link: &Path) -> std::io::Result<()> {
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
        assert_eq!(list.ends.len(), 4);
        assert_eq!(
            list.iter_rev().collect::<Vec<_>>(),
            ["c", "dir/sub/ünï.txt", "dir/b.txt", "a"]
        );
        assert_eq!(PathList::default().iter_rev().count(), 0);
    }
}

#[cfg(test)]
mod spool_name_tests {
    use super::spool_entry_name;

    #[test]
    fn deep_paths_get_short_unique_names() {
        let deep = format!("{}/deep.txt", vec!["d".repeat(40); 8].join("/"));
        assert!(deep.len() > 255);
        let first = spool_entry_name(&deep);
        let second = spool_entry_name(&deep);
        assert_ne!(first, second);
        for name in [&first, &second] {
            assert!(name.len() < 100, "{name}");
            assert!(name.ends_with("_deep.txt"), "{name}");
            assert!(!name.contains(['/', '\\']), "{name}");
        }
    }

    #[test]
    fn long_file_names_are_cut_on_a_char_boundary() {
        let long = "ü".repeat(120);
        let name = spool_entry_name(&format!("dir/{long}"));
        assert!(name.len() <= 100, "{name}");
        assert!(long.ends_with(name.rsplit('_').next().unwrap_or_default()));
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
