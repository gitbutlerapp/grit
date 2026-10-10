//! Temporary object directories for receive-pack (Git quarantine).
//!
//! Incoming push packs are unpacked into a quarantined `objects/` tree that lists
//! the repository's main object directory as an alternate. Hooks see
//! `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, and
//! `GIT_QUARANTINE_PATH`. On success the quarantine contents are migrated into
//! the main store; on failure the directory is removed.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::odb::Odb;

static QUARANTINE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A receive-pack quarantine directory under the repository's `objects/`.
pub struct ReceiveQuarantine {
    path: PathBuf,
    main_objects: PathBuf,
    migrated: bool,
}

impl ReceiveQuarantine {
    /// Create `tmp_objdir-incoming-*` under `main_objects` with a `pack/` subdir.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when the directory cannot be created.
    pub fn create(main_objects: &Path) -> Result<Self> {
        let seq = QUARANTINE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = main_objects.join(format!(
            "tmp_objdir-incoming-{}-{}",
            std::process::id(),
            seq
        ));
        fs::create_dir_all(path.join("pack")).map_err(Error::Io)?;
        Ok(Self {
            path,
            main_objects: main_objects.to_path_buf(),
            migrated: false,
        })
    }

    /// Quarantine `objects/` path (also `GIT_QUARANTINE_PATH`).
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Object database that reads/writes the quarantine and falls back to alternates.
    #[must_use]
    pub fn odb(&self) -> Odb {
        Odb::new(&self.path).with_env_alternate_dirs(vec![self.main_objects.clone()])
    }

    /// Environment pairs for hook subprocesses (absolute paths when possible).
    #[must_use]
    pub fn hook_env(&self) -> Vec<(String, String)> {
        let quarantine = abs_or_path(&self.path);
        let main = abs_or_path(&self.main_objects);
        vec![
            ("GIT_OBJECT_DIRECTORY".to_owned(), quarantine.clone()),
            ("GIT_ALTERNATE_OBJECT_DIRECTORIES".to_owned(), main),
            ("GIT_QUARANTINE_PATH".to_owned(), quarantine),
        ]
    }

    /// Move loose objects and packfiles from the quarantine into the main object store.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when migration fails.
    pub fn migrate(&mut self) -> Result<()> {
        migrate_tree(&self.path, &self.main_objects)?;
        self.migrated = true;
        remove_quarantine_dir(&self.path)?;
        Ok(())
    }

    /// Remove the quarantine directory without migrating.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when removal fails.
    pub fn discard(mut self) -> Result<()> {
        self.migrated = true;
        remove_quarantine_dir(&self.path)
    }
}

impl Drop for ReceiveQuarantine {
    fn drop(&mut self) {
        if !self.migrated {
            let _ = remove_quarantine_dir(&self.path);
        }
    }
}

fn abs_or_path(path: &Path) -> String {
    path.canonicalize()
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

fn remove_quarantine_dir(path: &Path) -> Result<()> {
    if path.is_dir() {
        fs::remove_dir_all(path).map_err(Error::Io)?;
    }
    Ok(())
}

fn migrate_tree(src: &Path, dst: &Path) -> Result<()> {
    if !src.is_dir() {
        return Ok(());
    }
    fs::create_dir_all(dst).map_err(Error::Io)?;
    let mut names: Vec<String> = fs::read_dir(src)
        .map_err(Error::Io)?
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort_by_key(|name| pack_copy_priority(name));
    for name in names {
        let from = src.join(&name);
        let to = dst.join(&name);
        let meta = fs::symlink_metadata(&from).map_err(Error::Io)?;
        if meta.is_dir() {
            migrate_tree(&from, &to)?;
        } else {
            migrate_file(&from, &to)?;
        }
    }
    Ok(())
}

fn pack_copy_priority(name: &str) -> u8 {
    if !name.starts_with("pack-") && name != "pack" {
        return 0;
    }
    if name.ends_with(".keep") {
        1
    } else if name.ends_with(".pack") {
        2
    } else if name.ends_with(".rev") {
        3
    } else if name.ends_with(".idx") {
        4
    } else {
        5
    }
}

fn migrate_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    if to.exists() {
        return Ok(());
    }
    if fs::rename(from, to).is_err() {
        fs::copy(from, to).map_err(Error::Io)?;
        fs::remove_file(from).map_err(Error::Io)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;

    #[test]
    fn quarantine_ingest_and_migrate_makes_objects_visible_in_main() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        fs::create_dir_all(&main).unwrap();
        let mut q = ReceiveQuarantine::create(&main).expect("quarantine");
        let oid = q
            .odb()
            .write(ObjectKind::Blob, b"quarantined")
            .expect("write");
        assert!(q.odb().exists(&oid));
        assert!(!Odb::new(&main).exists(&oid));
        q.migrate().expect("migrate");
        assert!(Odb::new(&main).exists(&oid));
        assert!(!q.path().exists());
    }

    #[test]
    fn discard_removes_quarantine_without_migrating() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        fs::create_dir_all(&main).unwrap();
        let q = ReceiveQuarantine::create(&main).expect("quarantine");
        let oid = q.odb().write(ObjectKind::Blob, b"x").expect("write");
        let path = q.path().to_path_buf();
        q.discard().expect("discard");
        assert!(!path.exists());
        assert!(!Odb::new(&main).exists(&oid));
    }
}
