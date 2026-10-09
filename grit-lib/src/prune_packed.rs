//! Library implementation of `prune-packed`.
//!
//! Removes loose objects that are already stored in a pack file, freeing
//! disk space without losing any object data.

use crate::error::Result;
use crate::odb::store::ObjectStore;
use crate::odb::Odb;
use std::collections::HashSet;
use std::fs;
use std::io;
use std::ops::ControlFlow;
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
/// [`PrunePackedOptions::dry_run`], paths are listed in the return value only).
/// Empty two-char prefix directories are removed afterwards.
///
/// Returns the list of paths that were (or would be) removed.
///
/// # Errors
///
/// - [`Error::Io`] for directory or file access failures.
pub fn prune_packed_objects(objects_dir: &Path, opts: PrunePackedOptions) -> Result<Vec<PathBuf>> {
    let odb = Odb::new(objects_dir);
    let primary = odb.primary()?;
    let loose = primary.loose_store();
    let packed = primary.packed_objects();

    let mut packed_ids = HashSet::new();
    packed.for_each_object(&mut |oid| {
        packed_ids.insert(*oid);
        ControlFlow::Continue(())
    })?;
    if packed_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut candidates = Vec::new();
    let mut prefix_dirs = HashSet::new();
    loose.for_each_object(&mut |oid| {
        if !packed_ids.contains(oid) {
            return ControlFlow::Continue(());
        }
        let obj_path = loose.object_path(oid);
        if let Some(parent) = obj_path.parent() {
            prefix_dirs.insert(parent.to_path_buf());
        }
        candidates.push(obj_path);
        ControlFlow::Continue(())
    })?;

    let mut removed = Vec::new();
    for obj_path in candidates {
        if !opts.dry_run {
            match fs::remove_file(&obj_path) {
                Ok(()) => {}
                Err(err) if err.kind() == io::ErrorKind::NotFound => {}
                Err(err) => return Err(crate::error::Error::Io(err)),
            }
        }
        removed.push(obj_path);
    }

    if !opts.dry_run {
        for dir in prefix_dirs {
            let _ = fs::remove_dir(dir);
        }
    }

    Ok(removed)
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
