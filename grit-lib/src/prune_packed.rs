//! Library implementation of `prune-packed`.
//!
//! Removes loose objects that are already stored in a pack file, freeing
//! disk space without losing any object data.

use crate::error::{Error, Result};
use crate::objects::{HashAlgo, ObjectId};
use crate::pack::read_local_pack_indexes;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// Options controlling the behaviour of [`prune_packed_objects`].
#[derive(Debug, Clone, Copy, Default)]
pub struct PrunePackedOptions {
    /// When `true`, print what would be deleted without actually deleting.
    pub dry_run: bool,
    /// When `true`, suppress informational output.
    pub quiet: bool,
}

/// Remove loose objects that are already stored in a pack file.
///
/// For each loose object under `objects_dir` whose [`ObjectId`] appears in
/// at least one local pack index, the file is deleted (or, with
/// [`PrunePackedOptions::dry_run`], the deletion command is printed to
/// `stdout`).  Empty two-char prefix directories are removed afterwards.
///
/// Returns the list of paths that were (or would be) removed.
///
/// # Errors
///
/// - [`Error::Io`] for directory or file access failures.
pub fn prune_packed_objects(objects_dir: &Path, opts: PrunePackedOptions) -> Result<Vec<PathBuf>> {
    let packed_ids = collect_packed_ids(objects_dir)?;
    if packed_ids.is_empty() {
        return Ok(Vec::new());
    }
    let hash_bytes = packed_ids
        .iter()
        .next()
        .map(|oid| oid.as_bytes().len())
        .unwrap_or(HashAlgo::Sha1.len());
    let loose_name_len = hash_bytes * 2 - 2;

    let mut removed = Vec::new();
    let rd = match fs::read_dir(objects_dir) {
        Ok(rd) => rd,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(Error::Io(err)),
    };

    for entry in rd {
        let entry = entry.map_err(Error::Io)?;
        let dir_name = entry.file_name().to_string_lossy().to_string();

        // Only process two-hex-char prefix subdirectories.
        if dir_name.len() != 2
            || !dir_name.chars().all(|c| c.is_ascii_hexdigit())
            || !entry.path().is_dir()
        {
            continue;
        }

        let sub_rd = match fs::read_dir(entry.path()) {
            Ok(rd) => rd,
            Err(err) if err.kind() == io::ErrorKind::NotFound => continue,
            Err(err) => return Err(Error::Io(err)),
        };

        for file in sub_rd {
            let file = file.map_err(Error::Io)?;
            let file_name = file.file_name().to_string_lossy().to_string();
            // Loose object filenames are (2 * hash_bytes - 2) hex chars after the prefix dir.
            if file_name.len() != loose_name_len
                || !file_name.chars().all(|c| c.is_ascii_hexdigit())
            {
                continue;
            }

            let hex = format!("{dir_name}{file_name}");
            let oid: ObjectId = match hex.parse() {
                Ok(id) => id,
                Err(_) => continue,
            };

            if !packed_ids.contains(&oid) {
                continue;
            }

            let obj_path = file.path();
            if opts.dry_run {
                println!("rm -f {}", obj_path.display());
            } else {
                match fs::remove_file(&obj_path) {
                    Ok(()) => {}
                    Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                    Err(err) => return Err(Error::Io(err)),
                }
            }
            removed.push(obj_path);
        }

        // Try to remove the now-possibly-empty prefix directory.
        if !opts.dry_run {
            let _ = fs::remove_dir(entry.path());
        }
    }

    Ok(removed)
}

/// Build the set of all object IDs present in local pack indexes.
fn collect_packed_ids(objects_dir: &Path) -> Result<HashSet<ObjectId>> {
    let indexes = read_local_pack_indexes(objects_dir)?;
    let mut ids = HashSet::new();
    for idx in indexes {
        for entry in idx.entries {
            if let Ok(oid) = ObjectId::from_bytes(&entry.oid) {
                ids.insert(oid);
            }
        }
    }
    Ok(ids)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use tempfile::TempDir;

    #[test]
    fn no_packs_leaves_loose_objects_intact() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let oid = odb.write(ObjectKind::Blob, b"hello").unwrap();

        let opts = PrunePackedOptions {
            dry_run: false,
            quiet: true,
        };
        let removed = prune_packed_objects(dir.path(), opts).unwrap();
        assert!(removed.is_empty());
        assert!(odb.exists(&oid));
    }

    #[test]
    fn dry_run_does_not_delete_files() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let oid = odb.write(ObjectKind::Blob, b"dry run test").unwrap();

        // No pack indexes — nothing would be pruned.
        let opts = PrunePackedOptions {
            dry_run: true,
            quiet: false,
        };
        let removed = prune_packed_objects(dir.path(), opts).unwrap();
        assert!(removed.is_empty());
        assert!(odb.exists(&oid));
    }
}
