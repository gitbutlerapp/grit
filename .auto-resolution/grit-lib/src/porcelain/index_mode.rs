//! Explicit index file modes for paths where the worktree does not report executable bits.
//!
//! When `core.filemode` is false (typical on Windows), [`crate::diff::mode_from_metadata`]
//! always yields a non-executable mode. Staging can still record `100755` in the index when
//! the caller sets an [`StageExecutableHint`], matching Git's `update-index --chmod` behavior.

use crate::error::{Error, Result};
use crate::index::{Index, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK, MODE_TREE};
use crate::repo::Repository;

/// Whether staging should force the executable bit for matched regular files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StageExecutableHint {
    /// Use worktree metadata (and preserve an existing index mode when `core.filemode` is false).
    #[default]
    None,
    /// Store [`MODE_EXECUTABLE`] for eligible regular files.
    SetExecutable,
    /// Store [`MODE_REGULAR`] for eligible regular files.
    SetNonExecutable,
}

/// Returns whether `mode` is a regular file mode that may be toggled executable/non-executable.
#[must_use]
pub(crate) fn is_chmod_eligible_mode(mode: u32) -> bool {
    mode == MODE_REGULAR || mode == MODE_EXECUTABLE
}

/// Apply an explicit staging hint to a base mode derived from the worktree or index.
#[must_use]
pub(crate) fn staged_mode_with_hint(base: u32, hint: StageExecutableHint) -> u32 {
    match hint {
        StageExecutableHint::None => base,
        StageExecutableHint::SetExecutable if is_chmod_eligible_mode(base) => MODE_EXECUTABLE,
        StageExecutableHint::SetNonExecutable if is_chmod_eligible_mode(base) => MODE_REGULAR,
        _ => base,
    }
}

/// Update the stage-0 mode for `path` without changing the object id or stat cache.
///
/// Rejects symlinks, gitlinks, directories, and unmerged paths, mirroring Git
/// `update-index --chmod`.
///
/// # Returns
///
/// `Ok(true)` when the entry existed and its mode changed; `Ok(false)` when there is no
/// stage-0 entry or the mode already matched.
///
/// # Errors
///
/// Returns [`Error::Message`] when the path is not a regular file entry or is unmerged.
pub fn set_stage0_file_mode(index: &mut Index, path: &[u8], mode: u32) -> Result<bool> {
    if index
        .entries
        .iter()
        .any(|e| e.path == path && e.stage() != 0)
    {
        return Err(Error::Message(format!(
            "cannot change mode of unmerged path '{}'",
            String::from_utf8_lossy(path)
        )));
    }
    let Some(entry) = index.get_mut(path, 0) else {
        return Ok(false);
    };
    if !is_chmod_eligible_mode(entry.mode) {
        return Err(Error::Message(format!(
            "cannot set executable bit on '{}'",
            String::from_utf8_lossy(path)
        )));
    }
    if !is_chmod_eligible_mode(mode) {
        return Err(Error::Message(format!(
            "invalid mode for regular file '{}'",
            String::from_utf8_lossy(path)
        )));
    }
    if entry.mode == mode {
        return Ok(false);
    }
    entry.mode = mode;
    index.invalidate_cache_tree_for_path(path);
    Ok(true)
}

/// Flip the executable bit on a tracked stage-0 regular file and write the index.
///
/// # Errors
///
/// Propagates index I/O failures and [`set_stage0_file_mode`] validation errors.
pub fn set_index_entry_executable(repo: &Repository, path: &str, executable: bool) -> Result<bool> {
    let mut index = repo.load_index()?;
    let target = if executable {
        MODE_EXECUTABLE
    } else {
        MODE_REGULAR
    };
    let changed = set_stage0_file_mode(&mut index, path.as_bytes(), target)?;
    if changed {
        repo.write_index(&mut index)?;
    }
    Ok(changed)
}

/// Apply [`StageExecutableHint`] to stage-0 entries matching `matches`.
///
/// Used by [`super::add::stage`] after content staging when pathspecs name explicit paths.
pub(crate) fn apply_executable_hint_to_index(
    index: &mut Index,
    hint: StageExecutableHint,
    matches: &impl Fn(&str) -> bool,
    dry_run: bool,
) -> Result<Vec<String>> {
    if hint == StageExecutableHint::None {
        return Ok(Vec::new());
    }
    let target = staged_mode_with_hint(MODE_REGULAR, hint);
    let mut updated = Vec::new();
    let paths: Vec<Vec<u8>> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && std::str::from_utf8(&e.path).ok().is_some_and(matches))
        .map(|e| e.path.clone())
        .collect();

    for path in paths {
        let rel = std::str::from_utf8(&path).unwrap_or("");
        if index
            .entries
            .iter()
            .any(|e| e.path == path && e.stage() != 0)
        {
            return Err(Error::Message(format!(
                "cannot change mode of unmerged path '{rel}'"
            )));
        }
        let Some(entry) = index.get(&path, 0) else {
            continue;
        };
        if entry.mode == MODE_SYMLINK || entry.mode == MODE_GITLINK || entry.mode == MODE_TREE {
            return Err(Error::Message(format!(
                "cannot set executable bit on '{rel}'"
            )));
        }
        if !is_chmod_eligible_mode(entry.mode) {
            continue;
        }
        if entry.mode == target {
            continue;
        }
        if dry_run {
            updated.push(rel.to_owned());
            continue;
        }
        if let Some(entry) = index.get_mut(&path, 0) {
            entry.mode = target;
            index.invalidate_cache_tree_for_path(&path);
            updated.push(rel.to_owned());
        }
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::IndexEntry;
    use crate::objects::ObjectId;

    fn regular_entry(path: &str, mode: u32) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 1,
            oid: ObjectId::zero(),
            flags: 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn hint_maps_regular_modes() {
        assert_eq!(
            staged_mode_with_hint(MODE_REGULAR, StageExecutableHint::SetExecutable),
            MODE_EXECUTABLE
        );
        assert_eq!(
            staged_mode_with_hint(MODE_EXECUTABLE, StageExecutableHint::SetNonExecutable),
            MODE_REGULAR
        );
        assert_eq!(
            staged_mode_with_hint(MODE_SYMLINK, StageExecutableHint::SetExecutable),
            MODE_SYMLINK
        );
    }

    #[test]
    fn set_stage0_file_mode_rejects_symlink() {
        let mut index = Index::new();
        index.add_or_replace(regular_entry("link", MODE_SYMLINK));
        let err = set_stage0_file_mode(&mut index, b"link", MODE_EXECUTABLE).unwrap_err();
        assert!(matches!(err, Error::Message(_)));
    }

    #[test]
    fn set_stage0_file_mode_updates_regular_file() {
        let mut index = Index::new();
        index.add_or_replace(regular_entry("run.sh", MODE_REGULAR));
        assert!(set_stage0_file_mode(&mut index, b"run.sh", MODE_EXECUTABLE).expect("chmod"));
        assert_eq!(
            index.get(b"run.sh", 0).expect("entry").mode,
            MODE_EXECUTABLE
        );
    }

    #[test]
    fn set_stage0_file_mode_rejects_unknown_entry_mode() {
        let mut index = Index::new();
        index.add_or_replace(regular_entry("x", 0o100664));
        let err = set_stage0_file_mode(&mut index, b"x", MODE_EXECUTABLE).unwrap_err();
        assert!(matches!(err, Error::Message(_)));
    }

    #[test]
    fn set_stage0_file_mode_rejects_invalid_target_mode() {
        let mut index = Index::new();
        index.add_or_replace(regular_entry("run.sh", MODE_REGULAR));
        let err = set_stage0_file_mode(&mut index, b"run.sh", MODE_SYMLINK).unwrap_err();
        assert!(matches!(err, Error::Message(_)));
    }
}
