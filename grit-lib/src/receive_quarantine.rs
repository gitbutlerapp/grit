//! Temporary object directories for receive-pack (Git quarantine).
//!
//! Incoming push packs are unpacked into a quarantined `objects/` tree that lists
//! the repository's main object directory as an alternate. Hooks see
//! `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, and
//! `GIT_QUARANTINE_PATH`. On success the quarantine contents are migrated into
//! the main store; on failure the directory is removed.

use crate::git_path::CanonicalizeExt;
use std::borrow::Cow;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Error, Result};
use crate::lockfile::{LockError, LockFile, LockWait};
use crate::odb::Odb;

static QUARANTINE_SEQ: AtomicU64 = AtomicU64::new(0);

/// How dropping a guard affects the on-disk `.keep` marker.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepMarkerRelease {
    /// This guard incremented a grit-managed decimal refcount in the marker file.
    GritRefcount,
    /// The marker predates this guard (empty or non-numeric); never unlink on drop.
    External,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum KeepMarkerState {
    Absent,
    External,
    Grit(u32),
}

fn keep_serialization_lock_path(keep_path: &Path) -> PathBuf {
    let name = keep_path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "pack.keep".to_owned());
    keep_path
        .parent()
        .map(|dir| dir.join(format!("{name}.lock")))
        .unwrap_or_else(|| PathBuf::from(format!("{name}.lock")))
}

fn lock_keep_marker(keep_path: &Path) -> std::result::Result<LockFile, Error> {
    LockFile::acquire(
        &keep_serialization_lock_path(keep_path),
        LockWait::Indefinitely,
    )
    .map_err(|err| match err {
        LockError::Io { source, .. } => Error::Io(source),
        LockError::Held { .. } => Error::Io(io::Error::other("pack keep lock held")),
    })
}

fn read_keep_marker_state(keep_path: &Path) -> io::Result<KeepMarkerState> {
    if !keep_path.is_file() {
        return Ok(KeepMarkerState::Absent);
    }
    let bytes = fs::read(keep_path)?;
    let text = String::from_utf8_lossy(&bytes);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(KeepMarkerState::External);
    }
    match trimmed.parse::<u32>() {
        Ok(0) => Ok(KeepMarkerState::External),
        Ok(count) => Ok(KeepMarkerState::Grit(count)),
        Err(_) => Ok(KeepMarkerState::External),
    }
}

fn write_grit_keep_refcount(keep_path: &Path, count: u32) -> io::Result<()> {
    fs::write(keep_path, count.to_string())
}

fn acquire_keep_marker(keep_path: &Path) -> Result<KeepMarkerRelease> {
    if let Some(parent) = keep_path.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let _lock = lock_keep_marker(keep_path)?;
    match read_keep_marker_state(keep_path)? {
        KeepMarkerState::Absent => {
            write_grit_keep_refcount(keep_path, 1).map_err(Error::Io)?;
            Ok(KeepMarkerRelease::GritRefcount)
        }
        KeepMarkerState::External => Ok(KeepMarkerRelease::External),
        KeepMarkerState::Grit(count) => {
            write_grit_keep_refcount(keep_path, count.saturating_add(1)).map_err(Error::Io)?;
            Ok(KeepMarkerRelease::GritRefcount)
        }
    }
}

fn release_keep_marker(keep_path: &Path, release: KeepMarkerRelease) {
    if release == KeepMarkerRelease::External {
        return;
    }
    let Ok(_lock) = lock_keep_marker(keep_path) else {
        return;
    };
    let Ok(state) = read_keep_marker_state(keep_path) else {
        return;
    };
    match state {
        KeepMarkerState::Grit(1) => {
            let _ = fs::remove_file(keep_path);
        }
        KeepMarkerState::Grit(count) => {
            let _ = write_grit_keep_refcount(keep_path, count - 1);
        }
        KeepMarkerState::Absent | KeepMarkerState::External => {}
    }
}

/// Keeps a pack reachable while refs that name its objects are still being published.
///
/// A sibling `.keep` marker is created before the pack file is visible in the main object
/// store. Grit-managed markers store a decimal refcount in the file and serialize updates
/// with a `{name}.keep.lock` file so separate receive-pack processes cannot unlink a marker
/// another process still holds. Pre-existing empty or non-numeric markers are left on disk.
pub struct MigratedPackKeepGuard {
    keep_path: PathBuf,
    release: KeepMarkerRelease,
}

impl MigratedPackKeepGuard {
    /// Create or join the `{stem}.keep` marker next to `pack_path`.
    fn install_for_pack(pack_path: &Path) -> Result<Self> {
        let keep_path = pack_keep_path(pack_path);
        let release = acquire_keep_marker(&keep_path)?;
        Ok(Self { keep_path, release })
    }
}

impl Drop for MigratedPackKeepGuard {
    fn drop(&mut self) {
        release_keep_marker(&self.keep_path, self.release);
    }
}

fn pack_keep_path(pack_path: &Path) -> PathBuf {
    let stem = pack_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("pack");
    pack_path
        .parent()
        .map(|p| p.join(format!("{stem}.keep")))
        .unwrap_or_else(|| PathBuf::from(format!("{stem}.keep")))
}

/// A receive-pack quarantine directory under the repository's `objects/`.
pub struct ReceiveQuarantine {
    path: PathBuf,
    main_objects: PathBuf,
    config_git_dir: Option<PathBuf>,
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
            migrated: false,
        })
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
    /// Returns [`MigratedPackKeepGuard`] values that must stay alive until ref updates from
    /// this push are committed, so a concurrent maintenance run cannot delete a newly visible
    /// pack before its tips are published.
    ///
    /// # Errors
    ///
    /// Returns [`Error::Io`] when migration fails.
    pub fn migrate(&mut self) -> Result<Vec<MigratedPackKeepGuard>> {
        let mut guards = Vec::new();
        migrate_tree(&self.path, &self.main_objects, &mut guards)?;
        self.migrated = true;
        remove_quarantine_dir(&self.path)?;
        Ok(guards)
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
    path.canonicalize_simplified()
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

fn migrate_tree(src: &Path, dst: &Path, guards: &mut Vec<MigratedPackKeepGuard>) -> Result<()> {
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
            migrate_tree(&from, &to, guards)?;
        } else if name.ends_with(".pack") {
            migrate_pack_file(&from, &to, guards)?;
        } else if name.ends_with(".keep") {
            // Receive-side `.keep` markers are not copied; migration installs its own guards.
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

fn migrate_pack_file(
    from: &Path,
    to: &Path,
    guards: &mut Vec<MigratedPackKeepGuard>,
) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    let guard = MigratedPackKeepGuard::install_for_pack(to)?;
    if to.exists() {
        guards.push(guard);
        let _ = fs::remove_file(from);
        return Ok(());
    }
    if fs::rename(from, to).is_err() {
        fs::copy(from, to).map_err(Error::Io)?;
        fs::remove_file(from).map_err(Error::Io)?;
    }
    guards.push(guard);
    Ok(())
}

fn migrate_file(from: &Path, to: &Path) -> Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent).map_err(Error::Io)?;
    }
    if to.exists() {
        let _ = fs::remove_file(from);
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
        let _keeps = q.migrate().expect("migrate");
        assert!(Odb::new(&main).exists(&oid));
    }

    #[test]
    fn migrate_pack_installs_keep_before_pack_is_visible() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        let pack_dir = main.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let main_odb = Odb::new(&main);
        let mut q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        let q_pack = q.path.join("pack/pack-test.pack");
        fs::write(&q_pack, b"PACK").unwrap();
        let guards = q.migrate().expect("migrate");
        assert_eq!(guards.len(), 1);
        assert!(pack_dir.join("pack-test.pack").is_file());
        assert!(pack_dir.join("pack-test.keep").is_file());
        drop(guards);
        assert!(!pack_dir.join("pack-test.keep").is_file());
    }

    #[test]
    fn migrate_pack_protects_existing_destination_pack() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        let pack_dir = main.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        fs::write(pack_dir.join("pack-dup.pack"), b"existing").unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let main_odb = Odb::new(&main);
        let mut q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        fs::write(q.path.join("pack/pack-dup.pack"), b"incoming").unwrap();
        let guards = q.migrate().expect("migrate");
        assert_eq!(guards.len(), 1);
        assert!(pack_dir.join("pack-dup.keep").is_file());
        drop(guards);
    }

    #[test]
    fn overlapping_migrations_retain_keep_until_last_guard_drops() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        let pack_dir = main.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let main_odb = Odb::new(&main);
        let mut first = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        fs::write(first.path.join("pack/pack-same.pack"), b"first").unwrap();
        let mut second = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        fs::write(second.path.join("pack/pack-same.pack"), b"second").unwrap();
        let first_guards = first.migrate().expect("first migrate");
        let second_guards = second.migrate().expect("second migrate");
        assert!(pack_dir.join("pack-same.keep").is_file());
        drop(first_guards);
        assert!(
            pack_dir.join("pack-same.keep").is_file(),
            "second live migration guard lost protection when first guard dropped"
        );
        drop(second_guards);
        assert!(!pack_dir.join("pack-same.keep").is_file());
    }

    #[test]
    fn migration_preserves_preexisting_external_keep_marker() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        let pack_dir = main.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        fs::write(pack_dir.join("pack-dup.pack"), b"existing").unwrap();
        fs::write(pack_dir.join("pack-dup.keep"), b"").unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let main_odb = Odb::new(&main);
        let mut q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        fs::write(q.path.join("pack/pack-dup.pack"), b"incoming").unwrap();
        let guards = q.migrate().expect("migrate");
        assert!(pack_dir.join("pack-dup.keep").is_file());
        drop(guards);
        assert!(
            pack_dir.join("pack-dup.keep").is_file(),
            "external keep marker must survive guard drop"
        );
    }

    const KEEP_CROSS_PROCESS_CHILD: &str = "GRIT_RECEIVE_KEEP_CROSS_PROCESS_CHILD";

    #[test]
    fn keep_marker_cross_process_child_holds_first_guard() {
        let Some(work) = std::env::var_os(KEEP_CROSS_PROCESS_CHILD) else {
            return;
        };
        let work = PathBuf::from(work);
        let main = work.join("objects");
        let git_dir = work.join(".git");
        let main_odb = Odb::new(&main);
        let mut q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        fs::write(q.path.join("pack/pack-same.pack"), b"first").unwrap();
        let guards = q.migrate().expect("migrate");
        fs::write(work.join("child-ready"), b"ok").unwrap();
        let mut line = String::new();
        let _ = std::io::stdin().read_line(&mut line);
        drop(guards);
    }

    #[test]
    fn keep_markers_outlive_first_process_while_second_holds_guard() {
        if std::env::var_os(KEEP_CROSS_PROCESS_CHILD).is_some() {
            return;
        }
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        let pack_dir = main.join("pack");
        fs::create_dir_all(&pack_dir).unwrap();
        let git_dir = tmp.path().join(".git");
        fs::create_dir_all(&git_dir).unwrap();
        let exe = std::env::current_exe().expect("test binary");
        let mut child = std::process::Command::new(&exe)
            .args([
                "receive_quarantine::tests::keep_marker_cross_process_child_holds_first_guard",
                "--exact",
                "--nocapture",
            ])
            .env(KEEP_CROSS_PROCESS_CHILD, tmp.path())
            .stdin(std::process::Stdio::piped())
            .spawn()
            .expect("spawn child receive");
        let ready = tmp.path().join("child-ready");
        for _ in 0..500 {
            if ready.is_file() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(ready.is_file(), "child did not install keep marker");
        assert!(pack_dir.join("pack-same.keep").is_file());
        let main_odb = Odb::new(&main);
        let mut second = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
        fs::write(second.path.join("pack/pack-same.pack"), b"second").unwrap();
        let second_guards = second.migrate().expect("second migrate");
        if let Some(mut stdin) = child.stdin.take() {
            use std::io::Write;
            writeln!(stdin, "release").expect("signal child");
        }
        assert!(
            child.wait().expect("wait child").success(),
            "child receive failed"
        );
        assert!(
            pack_dir.join("pack-same.keep").is_file(),
            "second process lost keep protection when first process exited"
        );
        drop(second_guards);
        assert!(!pack_dir.join("pack-same.keep").is_file());
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
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
    fn drop_removes_quarantine_without_migrating() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let main = tmp.path().join("objects");
        fs::create_dir_all(&main).unwrap();
        let main_odb = Odb::new(&main);
        let git_dir = tmp.path().join(".git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let quarantine_path = {
            let q = ReceiveQuarantine::create(&main_odb, &git_dir).expect("quarantine");
            let oid = q.odb().write(ObjectKind::Blob, b"x").expect("write");
            let path = q
                .hook_env()
                .into_iter()
                .find(|(k, _)| k == "GIT_QUARANTINE_PATH")
                .map(|(_, v)| PathBuf::from(v))
                .expect("quarantine path");
            drop(q);
            assert!(!path.exists());
            assert!(!Odb::new(&main).exists(&oid));
            path
        };
        let _ = quarantine_path;
    }
}
