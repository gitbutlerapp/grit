//! Temporary object directories for receive-pack (Git quarantine).
//!
//! Incoming push packs are unpacked into a quarantined `objects/` tree that lists
//! the repository's main object directory as an alternate. Hooks see
//! `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, and
//! `GIT_QUARANTINE_PATH`. On success the quarantine contents are migrated into
//! the main store; on failure the directory is removed.

use std::borrow::Cow;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::objects::HashAlgo;
use crate::odb::Odb;

static QUARANTINE_SEQ: AtomicU64 = AtomicU64::new(0);

/// A receive-pack quarantine directory under the repository's `objects/`.
pub struct ReceiveQuarantine {
    path: PathBuf,
    main_objects: PathBuf,
    config_git_dir: Option<PathBuf>,
    hash_algo: HashAlgo,
    migrated: bool,
}

impl ReceiveQuarantine {
    /// Create `tmp_objdir-incoming-*` under the repository's primary `objects/` tree.
    ///
    /// `source` supplies the hash algorithm and optional config git dir so pack
    /// ingestion matches the receiving repository (including SHA-256).
    ///
    /// # Errors
    ///
    /// Returns [`Error::UnsupportedObjectStore`] when `source` is not files-backed,
    /// or [`Error::Io`] when the directory cannot be created.
    pub fn create(source: &Odb, git_dir: &Path) -> Result<Self> {
        let main_objects = source
            .files_objects_dir()
            .ok_or(Error::UnsupportedObjectStore {
                operation: "receive quarantine",
            })?
            .to_path_buf();
        let seq = QUARANTINE_SEQ.fetch_add(1, Ordering::Relaxed);
        let path = main_objects.join(format!(
            "tmp_objdir-incoming-{}-{}",
            std::process::id(),
            seq
        ));
        fs::create_dir_all(path.join("pack")).map_err(Error::Io)?;
        Ok(Self {
            path,
            main_objects,
            config_git_dir: Some(git_dir.to_path_buf()),
            hash_algo: source.hash_algo(),
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
        let mut odb = Odb::new(&self.path).with_env_alternate_dirs(vec![self.main_objects.clone()]);
        if let Some(git_dir) = &self.config_git_dir {
            odb = odb.with_config_git_dir(git_dir.clone());
        }
        odb
    }

    /// Hash algorithm of the receiving repository.
    #[must_use]
    pub fn hash_algo(&self) -> HashAlgo {
        self.hash_algo
    }

    /// Environment pairs for hook subprocesses (absolute paths when possible).
    #[must_use]
    pub fn hook_env(&self) -> Vec<(String, String)> {
        let quarantine = abs_or_path(&self.path);
        let main = format_alternate_object_directories(&abs_or_path(&self.main_objects));
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

/// Format one path for `GIT_ALTERNATE_OBJECT_DIRECTORIES` (Git `env_append` quoting).
fn format_alternate_object_directories(path: &str) -> String {
    match quote_git_env_path(path) {
        Cow::Borrowed(p) => p.to_owned(),
        Cow::Owned(p) => p,
    }
}

fn quote_git_env_path(path: &str) -> Cow<'_, str> {
    if !path.contains(':') && !path.contains('"') {
        return Cow::Borrowed(path);
    }
    let mut out = String::from('"');
    for ch in path.chars() {
        match ch {
            '"' | '\\' => {
                out.push('\\');
                out.push(ch);
            }
            c if c.is_control() => {
                let b = c as u32;
                out.push_str(&format!("\\{b:03o}"));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    Cow::Owned(out)
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
        let main_odb = Odb::new(&main);
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let mut q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
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
    fn quarantine_odb_matches_repository_hash_algorithm() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let git_dir = tmp.path().join("repo.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let init = std::process::Command::new("git")
            .current_dir(&git_dir)
            .args(["init", "-q", "--bare", "--object-format=sha256", "."])
            .status();
        if init.map(|s| !s.success()).unwrap_or(true) {
            return;
        }
        let objects = git_dir.join("objects");
        let source = Odb::new(&objects).with_config_git_dir(git_dir.clone());
        assert_eq!(source.hash_algo(), crate::objects::HashAlgo::Sha256);
        let q = ReceiveQuarantine::create(&source, &git_dir).expect("quarantine");
        assert_eq!(q.odb().hash_algo(), crate::objects::HashAlgo::Sha256);
    }

    #[test]
    fn alternate_env_quotes_paths_containing_colons() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("obj:ects");
        std::fs::create_dir_all(&main).unwrap();
        let git_dir = tmp.path().join("repo.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let source = Odb::new(&main).with_config_git_dir(git_dir.clone());
        let q = ReceiveQuarantine::create(&source, &git_dir).expect("quarantine");
        let alt = q
            .hook_env()
            .into_iter()
            .find(|(k, _)| k == "GIT_ALTERNATE_OBJECT_DIRECTORIES")
            .map(|(_, v)| v)
            .expect("alt env");
        assert!(
            alt.starts_with('"') && alt.ends_with('"'),
            "colon in path must be quoted: {alt}"
        );
    }

    #[test]
    fn discard_removes_quarantine_without_migrating() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        fs::create_dir_all(&main).unwrap();
        let main_odb = Odb::new(&main);
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        let oid = q.odb().write(ObjectKind::Blob, b"x").expect("write");
        let path = q.path().to_path_buf();
        q.discard().expect("discard");
        assert!(!path.exists());
        assert!(!Odb::new(&main).exists(&oid));
    }
}
