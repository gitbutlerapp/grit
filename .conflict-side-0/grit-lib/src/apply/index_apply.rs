//! Apply unified patches to the Git index (`git apply --cached` semantics).

use super::blob_apply::{apply_patch_to_blob, BlobApplyOptions};
use super::FilePatch;
use crate::error::{ApplyError, Error};
use crate::index::{Index, IndexEntry, MODE_REGULAR};
use crate::objects::{ObjectId, ObjectKind};
use crate::repo::Repository;
use std::collections::BTreeMap;

/// Options for [`apply_patch_to_index`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IndexApplyOptions {
    /// When true, validate and report paths without writing the index or objects.
    pub dry_run: bool,
}

/// Result of applying patches to the index.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IndexApplyOutcome {
    /// New paths added to the index.
    pub added: Vec<String>,
    /// Existing paths updated in the index.
    pub modified: Vec<String>,
    /// Paths removed from the index.
    pub removed: Vec<String>,
    /// Whether this was a dry run.
    pub dry_run: bool,
}

impl IndexApplyOutcome {
    fn push_added(&mut self, path: String) {
        if !self.added.contains(&path) {
            self.added.push(path);
        }
    }

    fn push_modified(&mut self, path: String) {
        if !self.modified.contains(&path) && !self.added.contains(&path) {
            self.modified.push(path);
        }
    }

    fn push_removed(&mut self, path: String) {
        if !self.removed.contains(&path) {
            self.removed.push(path);
        }
    }

    fn finalize(mut self) -> Self {
        self.added.sort();
        self.modified.sort();
        self.removed.sort();
        self
    }
}

#[derive(Clone)]
enum PlannedChange {
    Upsert {
        path: String,
        oid: ObjectId,
        mode: u32,
        blob: Vec<u8>,
    },
    Remove {
        path: String,
    },
}

/// Apply `patches` to `index` using index blobs as the preimage (never the worktree).
///
/// The operation is atomic: every patch is validated before any blob or index entry is written.
pub fn apply_patch_to_index(
    repo: &Repository,
    index: &mut Index,
    patches: &[FilePatch],
    opts: &IndexApplyOptions,
) -> std::result::Result<IndexApplyOutcome, ApplyError> {
    let mut trial = index.clone();
    let mut outcome = IndexApplyOutcome {
        dry_run: opts.dry_run,
        ..IndexApplyOutcome::default()
    };
    let mut fn_table: BTreeMap<String, FnState> = BTreeMap::new();
    let mut planned = Vec::new();

    for patch in patches {
        if patch.involves_gitlink() {
            let path = patch.effective_path().unwrap_or("<unknown>").to_string();
            return Err(ApplyError::Failed {
                path,
                message: "submodule/gitlink patches are not supported".to_string(),
            });
        }
        let changes = plan_one_file_patch(repo, &trial, patch, &mut fn_table, &mut outcome)?;
        for change in &changes {
            apply_planned_to_index(&mut trial, change);
            planned.push(change.clone());
        }
    }

    if !opts.dry_run {
        for change in &planned {
            if let PlannedChange::Upsert { blob, .. } = change {
                repo.odb()
                    .write(ObjectKind::Blob, blob)
                    .map_err(|e| library_err_path("", e))?;
            }
        }
        *index = trial;
    }
    Ok(outcome.finalize())
}

/// Load the index, apply `patches`, and commit the updated index under `.git/index.lock`.
pub fn apply_patch_to_index_and_write(
    repo: &Repository,
    patches: &[FilePatch],
    opts: &IndexApplyOptions,
) -> std::result::Result<IndexApplyOutcome, ApplyError> {
    let handle = repo.index_handle();
    let mut lock = handle.begin_update().map_err(|e| library_err_path("", e))?;
    let mut index = handle.load().map_err(|e| library_err_path("", e))?;
    let outcome = apply_patch_to_index(repo, &mut index, patches, opts)?;
    if opts.dry_run {
        return Ok(outcome);
    }
    handle
        .commit_update(&mut lock, &mut index, false)
        .map_err(|e| library_err_path("", e))?;
    Ok(outcome)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FnState {
    Active,
    Deleted,
}

fn plan_one_file_patch(
    repo: &Repository,
    index: &Index,
    patch: &FilePatch,
    fn_table: &mut BTreeMap<String, FnState>,
    outcome: &mut IndexApplyOutcome,
) -> std::result::Result<Vec<PlannedChange>, ApplyError> {
    let target = patch
        .target_path()
        .map(str::to_string)
        .or_else(|| patch.effective_path().map(str::to_string))
        .ok_or_else(|| ApplyError::Failed {
            path: String::new(),
            message: "patch has no target path".to_string(),
        })?;

    if patch.is_deleted {
        let base_blob = read_preimage_blob(repo, index, patch, &target)?;
        apply_patch_to_blob(&base_blob, patch, &BlobApplyOptions::default())?;
        if index.get(target.as_bytes(), 0).is_none() {
            return Err(ApplyError::NotInIndex {
                path: target.clone(),
            });
        }
        fn_table.insert(target.clone(), FnState::Deleted);
        outcome.push_removed(target.clone());
        return Ok(vec![PlannedChange::Remove { path: target }]);
    }

    if patch.is_new
        && index.get(target.as_bytes(), 0).is_some()
        && fn_table.get(&target).copied() != Some(FnState::Deleted)
    {
        return Err(ApplyError::Failed {
            path: target.clone(),
            message: "path already exists in index".to_string(),
        });
    }

    let base_blob = read_preimage_blob(repo, index, patch, &target)?;
    let new_blob = apply_patch_to_blob(&base_blob, patch, &BlobApplyOptions::default())?;
    let mode = resolve_mode(index, patch, &target)?;
    let oid = hash_blob(repo, &new_blob)?;

    let mut out = Vec::new();
    if patch.is_rename {
        if let Some(old) = patch.source_path().map(str::to_string) {
            if old != target
                && fn_table.get(&old).copied() != Some(FnState::Deleted)
                && index.get(old.as_bytes(), 0).is_some()
            {
                fn_table.insert(old.clone(), FnState::Deleted);
                outcome.push_removed(old.clone());
                out.push(PlannedChange::Remove { path: old });
            }
        }
    }

    let existed = index.get(target.as_bytes(), 0).is_some()
        || fn_table.get(&target).copied() == Some(FnState::Active);
    if existed {
        outcome.push_modified(target.clone());
    } else {
        outcome.push_added(target.clone());
    }
    fn_table.insert(target.clone(), FnState::Active);

    out.push(PlannedChange::Upsert {
        path: target,
        oid,
        mode,
        blob: new_blob,
    });
    Ok(out)
}

fn apply_planned_to_index(index: &mut Index, change: &PlannedChange) {
    match change {
        PlannedChange::Remove { path } => {
            index.remove_path_all_stages(path.as_bytes());
        }
        PlannedChange::Upsert {
            path, oid, mode, ..
        } => {
            upsert_index_entry(index, path, *oid, *mode);
        }
    }
}

fn read_preimage_blob(
    repo: &Repository,
    index: &Index,
    patch: &FilePatch,
    target: &str,
) -> std::result::Result<Vec<u8>, ApplyError> {
    if patch.is_new {
        return Ok(Vec::new());
    }
    let path = patch
        .source_path()
        .or_else(|| patch.effective_path())
        .unwrap_or(target);
    if path == "/dev/null" {
        return Ok(Vec::new());
    }
    let entry = index
        .get(path.as_bytes(), 0)
        .ok_or_else(|| ApplyError::NotInIndex {
            path: path.to_string(),
        })?;
    repo.odb()
        .read(&entry.oid)
        .map(|o| o.data)
        .map_err(|e| ApplyError::Failed {
            path: path.to_string(),
            message: e.to_string(),
        })
}

fn resolve_mode(
    index: &Index,
    patch: &FilePatch,
    target: &str,
) -> std::result::Result<u32, ApplyError> {
    if let Some(mode) = patch.new_mode.as_deref() {
        return parse_mode(mode);
    }
    if patch.is_rename || patch.is_copy {
        if let Some(src) = patch
            .source_path()
            .filter(|p| *p != "/dev/null")
            .or_else(|| patch.old_path.as_deref().filter(|p| *p != "/dev/null"))
        {
            if let Some(entry) = index.get(src.as_bytes(), 0) {
                return Ok(entry.mode);
            }
        }
    }
    if let Some(mode) = patch.old_mode.as_deref() {
        return parse_mode(mode);
    }
    if let Some(entry) = index.get(target.as_bytes(), 0) {
        return Ok(entry.mode);
    }
    Ok(MODE_REGULAR)
}

fn parse_mode(mode: &str) -> std::result::Result<u32, ApplyError> {
    let trimmed = mode.trim();
    u32::from_str_radix(trimmed, 8).map_err(|e| ApplyError::Failed {
        path: String::new(),
        message: format!("invalid mode '{trimmed}': {e}"),
    })
}

fn hash_blob(repo: &Repository, data: &[u8]) -> std::result::Result<ObjectId, ApplyError> {
    repo.odb()
        .hash_algo()
        .hash_object(ObjectKind::Blob, data)
        .map_err(|e| ApplyError::Failed {
            path: String::new(),
            message: e.to_string(),
        })
}

fn upsert_index_entry(index: &mut Index, path: &str, oid: ObjectId, mode: u32) {
    let path_bytes = path.as_bytes().to_vec();
    let flags = (path.len().min(0xfff) as u16) & 0x0fff;
    let entry = IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: if mode == 0 { MODE_REGULAR } else { mode },
        uid: 0,
        gid: 0,
        size: 0,
        oid,
        flags,
        flags_extended: None,
        path: path_bytes,
        base_index_pos: 0,
    };
    index.add_or_replace(entry);
}

fn library_err_path(path: &str, err: Error) -> ApplyError {
    ApplyError::Failed {
        path: path.to_string(),
        message: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply::parse_patch;
    use crate::config::environment::RepositoryOptions;
    use crate::repo::Repository;
    use std::process::Command;

    fn init_repo(dir: &std::path::Path) {
        Command::new("git")
            .current_dir(dir)
            .args(["init", "-q", "-b", "main"])
            .status()
            .unwrap();
    }

    #[test]
    fn index_apply_modify() {
        let dir = tempfile::tempdir().unwrap();
        init_repo(dir.path());
        std::fs::write(dir.path().join("f.txt"), b"a\n").unwrap();
        Command::new("git")
            .current_dir(dir.path())
            .args(["add", "f.txt"])
            .status()
            .unwrap();
        let patch = parse_patch(
            "diff --git a/f.txt b/f.txt\n\
             --- a/f.txt\n\
             +++ b/f.txt\n\
             @@ -1 +1 @@\n\
             -a\n\
             +b\n",
            1,
            "p",
            false,
            None,
        )
        .unwrap();
        let repo = Repository::discover(Some(dir.path()), &RepositoryOptions::default()).unwrap();
        let mut index = repo.index_handle().load().unwrap();
        apply_patch_to_index(&repo, &mut index, &patch, &IndexApplyOptions::default()).unwrap();
        let entry = index.get(b"f.txt", 0).unwrap();
        let blob = repo.odb().read(&entry.oid).unwrap();
        assert_eq!(blob.data, b"b\n");
        assert!(entry.stat_cache_unset());
    }
}
