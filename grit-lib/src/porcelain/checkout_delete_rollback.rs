//! Spool-backed rollback for checkout file deletions (bounded memory, cross-device safe).

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use crate::error::{Error, Result};
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

/// Tracks worktree files removed during checkout so a later failure can restore them.
pub(crate) struct CheckoutDeleteRollback {
    spool: tempfile::TempDir,
    entries: Vec<SpooledCheckoutDelete>,
}

impl CheckoutDeleteRollback {
    /// Create a private spool directory (respects `TMPDIR`).
    pub(crate) fn new() -> Result<Self> {
        Ok(Self {
            spool: tempfile::Builder::new()
                .prefix("grit-checkout-del-")
                .tempdir()
                .map_err(Error::Io)?,
            entries: Vec::new(),
        })
    }

    /// Move `rel_path` out of the worktree into the spool, or no-op when absent.
    pub(crate) fn spool_remove(&mut self, work_tree: &Path, rel_path: &str) -> Result<()> {
        // Stored paths hex-encode non-UTF-8 names; decode them to the on-disk bytes.
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

    /// Restore spooled paths after a failed checkout (newest deletions first).
    pub(crate) fn rollback(&self, work_tree: &Path) {
        for entry in self.entries.iter().rev() {
            let _ = restore_spooled_entry(work_tree, entry);
        }
    }
}

/// Owns checkout delete rollback for one checkout pass; rolls back on drop unless [`Self::commit`] ran.
pub(crate) struct CheckoutDeleteGuard {
    rollback: Arc<Mutex<CheckoutDeleteRollback>>,
    work_tree: PathBuf,
    committed: bool,
}

impl CheckoutDeleteGuard {
    pub(crate) fn new(work_tree: PathBuf) -> Result<Self> {
        Ok(Self {
            rollback: Arc::new(Mutex::new(CheckoutDeleteRollback::new()?)),
            work_tree,
            committed: false,
        })
    }

    pub(crate) fn shared(&self) -> SharedCheckoutDeleteRollback {
        SharedCheckoutDeleteRollback::from_arc(Arc::clone(&self.rollback))
    }

    pub(crate) fn commit_success(&mut self) {
        self.committed = true;
        if let Ok(mut rb) = self.rollback.lock() {
            rb.entries.clear();
        }
    }
}

impl Drop for CheckoutDeleteGuard {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        if let Ok(rb) = self.rollback.lock() {
            rb.rollback(&self.work_tree);
        }
    }
}

/// Shared rollback for parallel checkout deletes.
pub(crate) struct SharedCheckoutDeleteRollback(Arc<Mutex<CheckoutDeleteRollback>>);

impl SharedCheckoutDeleteRollback {
    pub(crate) fn from_arc(arc: Arc<Mutex<CheckoutDeleteRollback>>) -> Self {
        Self(arc)
    }

    pub(crate) fn clone_handle(&self) -> Self {
        Self(Arc::clone(&self.0))
    }

    pub(crate) fn spool_remove(&self, work_tree: &Path, rel_path: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .spool_remove(work_tree, rel_path)
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
