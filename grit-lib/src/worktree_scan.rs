//! Directory-oriented worktree stat reads for index comparison.
//!
//! Groups indexed blob paths by parent directory and uses one `read_dir` per directory,
//! reusing `DirEntry` metadata for tracked files instead of a separate `symlink_metadata`
//! per index path.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry, MODE_GITLINK};

/// A tracked blob path grouped under its parent directory (repository-relative).
#[derive(Debug, Clone)]
pub(crate) struct BlobInDir {
    /// File name within the directory (no slashes).
    pub file_name: String,
    /// Full repository-relative path.
    pub rel_path: String,
    /// Index into [`Index::entries`].
    pub entry_index: usize,
}

/// Group stage-0 blob index entries by parent directory for bulk `read_dir`.
#[must_use]
pub(crate) fn group_blob_entries_by_dir(index: &Index) -> HashMap<String, Vec<BlobInDir>> {
    let mut by_dir: HashMap<String, Vec<BlobInDir>> = HashMap::new();
    for (entry_index, ie) in index.entries.iter().enumerate() {
        if ie.stage() != 0 {
            continue;
        }
        if ie.skip_worktree() || ie.assume_unchanged() || ie.intent_to_add() {
            continue;
        }
        if ie.mode == MODE_GITLINK {
            continue;
        }
        let Ok(rel_path) = std::str::from_utf8(&ie.path) else {
            continue;
        };
        let (dir, file_name) = split_dir_file(rel_path);
        by_dir.entry(dir).or_default().push(BlobInDir {
            file_name,
            rel_path: rel_path.to_owned(),
            entry_index,
        });
    }
    by_dir
}

fn split_dir_file(rel_path: &str) -> (String, String) {
    match rel_path.rsplit_once('/') {
        Some((dir, file)) => (dir.to_owned(), file.to_owned()),
        None => (String::new(), rel_path.to_owned()),
    }
}

/// Result of looking up one indexed blob on disk during a directory scan.
pub(crate) enum BlobDiskLookup {
    /// Metadata for the indexed path.
    Present(fs::Metadata),
    /// Missing path or non-directory parent (`NotFound` / `ENOTDIR`).
    Missing,
    /// Other I/O error.
    Io(io::Error),
}

/// Visit grouped blob entries with one `read_dir` per distinct parent directory.
///
/// When `has_symlink_ancestor(rel_path)` is true, the entry is reported as [`BlobDiskLookup::Missing`]
/// without extra I/O (symlink replaced a directory on the path).
pub(crate) fn for_each_blob_by_directory<F>(
    index: &Index,
    by_dir: &HashMap<String, Vec<BlobInDir>>,
    dir_abs: impl Fn(&str) -> PathBuf,
    file_abs: impl Fn(&str) -> PathBuf,
    mut has_symlink_ancestor: impl FnMut(&str) -> bool,
    mut visit: F,
) -> Result<()>
where
    F: FnMut(usize, &IndexEntry, &str, BlobDiskLookup) -> Result<()>,
{
    let mut dirs: Vec<_> = by_dir.keys().cloned().collect();
    dirs.sort();
    for dir in dirs {
        let Some(blobs) = by_dir.get(&dir) else {
            continue;
        };
        // When a tracked directory is replaced by a symlink, Git reports indexed paths as
        // deleted without traversing the link target (even if the target is unreadable).
        if blobs.iter().all(|b| has_symlink_ancestor(&b.rel_path)) {
            for blob in blobs {
                let ie = &index.entries[blob.entry_index];
                visit(
                    blob.entry_index,
                    ie,
                    &blob.rel_path,
                    BlobDiskLookup::Missing,
                )?;
            }
            continue;
        }

        let abs_dir = dir_abs(&dir);
        let (dir_readable, on_disk) = read_directory_map(&abs_dir)?;
        for blob in blobs {
            let ie = &index.entries[blob.entry_index];
            if has_symlink_ancestor(&blob.rel_path) {
                visit(
                    blob.entry_index,
                    ie,
                    &blob.rel_path,
                    BlobDiskLookup::Missing,
                )?;
                continue;
            }
            let target = file_abs(&blob.rel_path);
            let lookup = if !dir_readable {
                BlobDiskLookup::Missing
            } else {
                lookup_blob_in_directory(&on_disk, &blob.file_name, &target)
            };
            visit(blob.entry_index, ie, &blob.rel_path, lookup)?;
        }
    }
    Ok(())
}

fn lookup_blob_in_directory(
    on_disk: &HashMap<String, fs::DirEntry>,
    index_file_name: &str,
    target_abs: &Path,
) -> BlobDiskLookup {
    let try_entry = |entry: &fs::DirEntry| -> Option<std::result::Result<fs::Metadata, io::Error>> {
        if entry.path() == target_abs {
            Some(entry.metadata())
        } else {
            None
        }
    };

    if let Some(entry) = on_disk.get(index_file_name) {
        if let Some(meta) = try_entry(entry) {
            return match meta {
                Ok(m) => BlobDiskLookup::Present(m),
                Err(e) => BlobDiskLookup::Io(e),
            };
        }
    }
    for entry in on_disk.values() {
        if let Some(meta) = try_entry(entry) {
            return match meta {
                Ok(m) => BlobDiskLookup::Present(m),
                Err(e) => BlobDiskLookup::Io(e),
            };
        }
    }
    match fs::symlink_metadata(target_abs) {
        Ok(meta) => BlobDiskLookup::Present(meta),
        Err(e) if e.kind() == io::ErrorKind::NotFound || e.raw_os_error() == Some(20) => {
            BlobDiskLookup::Missing
        }
        Err(e) => BlobDiskLookup::Io(e),
    }
}

fn read_directory_map(abs_dir: &Path) -> Result<(bool, HashMap<String, fs::DirEntry>)> {
    match fs::read_dir(abs_dir) {
        Ok(rd) => {
            let mut on_disk = HashMap::new();
            for item in rd {
                let Ok(entry) = item else { continue };
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".git" {
                    continue;
                }
                on_disk.insert(name, entry);
            }
            Ok((true, on_disk))
        }
        Err(e)
            if e.kind() == io::ErrorKind::NotFound
                || e.kind() == io::ErrorKind::PermissionDenied
                || e.raw_os_error() == Some(20) =>
        {
            Ok((false, HashMap::new()))
        }
        Err(e) => Err(Error::Io(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::{entry_from_stat, Index};
    use crate::objects::{ObjectId, ObjectKind};
    use crate::odb::Odb;
    use std::fs;
    use tempfile::TempDir;

    fn write_blob_file(dir: &Path, rel: &str, bytes: &[u8]) -> ObjectId {
        let odb = Odb::new(&dir.join(".git/objects"));
        let abs = dir.join(rel);
        if let Some(parent) = abs.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&abs, bytes).unwrap();
        odb.write(ObjectKind::Blob, bytes).unwrap()
    }

    #[test]
    fn dir_grouped_scan_matches_clean_tree() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "d0/f0.txt", b"hello\n");
        let mut index = Index::new();
        index.add_or_replace(
            entry_from_stat(&wt.join("d0/f0.txt"), b"d0/f0.txt", oid, 0o100644).unwrap(),
        );
        let grouped = group_blob_entries_by_dir(&index);
        assert_eq!(grouped.len(), 1);
        assert!(grouped.contains_key("d0"));

        let mut seen = 0;
        let dir_abs = |dir: &str| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                wt.join(dir)
            }
        };
        let file_abs = |rel: &str| wt.join(rel);
        for_each_blob_by_directory(
            &index,
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            |_, ie, path, lookup| {
                assert_eq!(path, "d0/f0.txt");
                assert_eq!(ie.oid, oid);
                assert!(matches!(lookup, BlobDiskLookup::Present(_)));
                seen += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(seen, 1);
    }

    #[test]
    fn dir_grouped_scan_reports_missing_file() {
        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = ObjectId::zero();
        let mut index = Index::new();
        fs::write(wt.join("gone.txt"), b"x").unwrap();
        let entry = entry_from_stat(&wt.join("gone.txt"), b"gone.txt", oid, 0o100644).unwrap();
        index.add_or_replace(entry);
        fs::remove_file(wt.join("gone.txt")).unwrap();

        let grouped = group_blob_entries_by_dir(&index);
        let mut missing = false;
        let dir_abs = |dir: &str| {
            if dir.is_empty() {
                wt.to_path_buf()
            } else {
                wt.join(dir)
            }
        };
        let file_abs = |rel: &str| wt.join(rel);
        for_each_blob_by_directory(
            &index,
            &grouped,
            dir_abs,
            file_abs,
            |_| false,
            |_, _, _, lookup| {
                missing = matches!(lookup, BlobDiskLookup::Missing);
                Ok(())
            },
        )
        .unwrap();
        assert!(missing);
    }

    #[cfg(unix)]
    #[test]
    fn directory_scan_symlink_to_unreadable_target_reports_missing() {
        use crate::diff::{
            diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus,
        };
        use std::os::unix::fs::PermissionsExt;

        let tmp = TempDir::new().unwrap();
        let wt = tmp.path();
        fs::create_dir_all(wt.join(".git/objects")).unwrap();
        let oid = write_blob_file(wt, "dir/file.txt", b"tracked\n");
        let mut index = Index::new();
        index.add_or_replace(
            entry_from_stat(&wt.join("dir/file.txt"), b"dir/file.txt", oid, 0o100644).unwrap(),
        );

        fs::remove_dir_all(wt.join("dir")).unwrap();
        let secret = wt.join("secret");
        fs::create_dir(&secret).unwrap();
        fs::write(secret.join("file.txt"), b"other\n").unwrap();
        fs::set_permissions(&secret, fs::Permissions::from_mode(0)).unwrap();
        std::os::unix::fs::symlink(&secret, wt.join("dir")).unwrap();

        let odb = Odb::new(&wt.join(".git/objects"));
        let diffs = diff_index_to_worktree_with_options(
            &odb,
            &index,
            wt,
            DiffIndexToWorktreeOptions {
                index_mtime: None,
                ignore_submodule_untracked: false,
                simplify_gitlinks: false,
                error_on_broken_gitlinks: false,
            },
        )
        .expect("status diff must not traverse unreadable symlink target");
        assert_eq!(diffs.len(), 1);
        assert_eq!(diffs[0].status, DiffStatus::Deleted);
        assert_eq!(diffs[0].old_path.as_deref(), Some("dir/file.txt"));
    }
}
