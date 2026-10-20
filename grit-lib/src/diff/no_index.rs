//! Compare arbitrary filesystem paths without a repository (`git diff --no-index`).

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::ConfigSet;
use crate::index::MODE_REGULAR;
use crate::objects::{ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::parallel::ThreadPolicy;

use super::core::{
    apply_rename_detection_to_diff_entries, format_mode, mode_from_metadata,
    read_worktree_bytes_for_mode, zero_oid, DiffEntry, DiffStatus,
};
use super::options::DiffOptions;
use super::structured::{structured_file_diff, SideBytes, StructuredHunk};

/// Failure modes specific to [`diff_paths`].
#[derive(Debug, Error, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum NoIndexError {
    /// A path could not be read (missing or permission denied).
    #[error("Could not access '{path}'")]
    PathNotAccessible {
        /// Filesystem path that could not be accessed.
        path: String,
    },
}

impl From<NoIndexError> for crate::error::Error {
    fn from(value: NoIndexError) -> Self {
        Self::Message(value.to_string())
    }
}

/// One changed path pair from a no-index diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Display path (new path, or old path for a deletion).
    pub path: String,
    /// Pre-rename path when it differs from [`Self::path`].
    pub old_path: Option<String>,
    /// Change kind for this path.
    pub status: DiffStatus,
    /// Old git mode (octal), when present on this side.
    pub old_mode: Option<String>,
    /// New git mode (octal), when present on this side.
    pub new_mode: Option<String>,
    /// Old blob OID on this side (zero when absent).
    pub old_oid: ObjectId,
    /// New blob OID on this side (zero when absent).
    pub new_oid: ObjectId,
    /// True when either side looks like a binary blob (NUL in the first 8 KiB).
    pub binary: bool,
    /// True when blob content changed (not a mode-only touch on the same OID).
    pub content_changed: bool,
    /// Old side was decoded with lossy UTF-8.
    pub old_encoding_lossy: bool,
    /// New side was decoded with lossy UTF-8.
    pub new_encoding_lossy: bool,
    /// Structured line hunks for text files.
    pub hunks: Vec<StructuredHunk>,
}

/// Diff two filesystem paths (files, directories, or one of each).
///
/// Does not require a [`Repository`](crate::repo::Repository). Directory pairs are walked
/// recursively with sorted names; a file paired with a directory compares against
/// `directory/basename(file)` like Git. Symlink targets are diffed as text; executable
/// bit differences appear as mode changes.
///
/// # Errors
///
/// Returns [`NoIndexError::PathNotAccessible`] when either top-level path cannot be accessed.
pub fn diff_paths(a: &Path, b: &Path, opts: &DiffOptions) -> Result<Vec<FileDiff>, NoIndexError> {
    let (left, right) = fixup_paths(a, b)?;
    let mut entries = Vec::new();
    queue_diff(left.as_deref(), right.as_deref(), false, &mut entries)?;
    if entries.is_empty() {
        return Ok(Vec::new());
    }

    let config = ConfigSet::default();
    let thread_policy = ThreadPolicy::default();
    let rename_threshold = opts.renames.threshold_percent().unwrap_or(101);
    let temp = tempfile::tempdir().map_err(|e| NoIndexError::PathNotAccessible {
        path: e.to_string(),
    })?;
    let odb = Odb::new(temp.path());
    materialize_no_index_oids(&odb, &mut entries)?;
    let entries = if rename_threshold < 100 {
        apply_rename_detection_to_diff_entries(
            &odb,
            entries,
            None,
            rename_threshold,
            &config,
            &thread_policy,
        )
        .map_err(|e| NoIndexError::PathNotAccessible {
            path: e.to_string(),
        })?
    } else {
        entries
    };

    let diff_options = opts.cap_word_level_inline_for_changed_files(entries.len());
    Ok(entries
        .into_iter()
        .filter_map(|entry| file_diff_from_entry(&odb, &entry, diff_options))
        .collect())
}

fn file_diff_from_entry(odb: &Odb, entry: &DiffEntry, opts: DiffOptions) -> Option<FileDiff> {
    if entry.old_oid == entry.new_oid && entry.old_mode == entry.new_mode {
        return None;
    }
    let (old_bytes, old_bin) = side_bytes_for_entry(odb, entry, Side::Old)?;
    let (new_bytes, new_bin) = side_bytes_for_entry(odb, entry, Side::New)?;
    let binary = old_bin || new_bin;
    let path = entry
        .old_path
        .clone()
        .or_else(|| entry.new_path.clone())
        .unwrap_or_default();
    let old_path = entry
        .old_path
        .clone()
        .filter(|op| entry.new_path.as_ref().is_some_and(|np| np != op));
    let structured = if binary {
        super::structured::StructuredFileDiff {
            old_mode: mode_display(&entry.old_mode),
            new_mode: mode_display(&entry.new_mode),
            old_encoding_lossy: old_bytes.encoding_lossy,
            new_encoding_lossy: new_bytes.encoding_lossy,
            hunks: Vec::new(),
        }
    } else {
        structured_file_diff(
            &entry.old_mode,
            &entry.new_mode,
            &old_bytes,
            &new_bytes,
            opts,
        )
    };
    Some(FileDiff {
        path,
        old_path,
        status: entry.status,
        old_mode: mode_display(&entry.old_mode),
        new_mode: mode_display(&entry.new_mode),
        old_oid: entry.old_oid,
        new_oid: entry.new_oid,
        binary,
        content_changed: entry.old_oid != entry.new_oid,
        old_encoding_lossy: structured.old_encoding_lossy,
        new_encoding_lossy: structured.new_encoding_lossy,
        hunks: structured.hunks,
    })
}

enum Side {
    Old,
    New,
}

fn side_bytes_for_entry(odb: &Odb, entry: &DiffEntry, side: Side) -> Option<(SideBytes, bool)> {
    let (mode, oid) = match side {
        Side::Old => (&entry.old_mode, entry.old_oid),
        Side::New => (&entry.new_mode, entry.new_oid),
    };
    if *mode == "000000" {
        return Some((SideBytes::from_bytes(Vec::new()), false));
    }
    let data = odb.read(&oid).ok()?.data;
    Some(bytes_for_diff(&data))
}

fn bytes_for_diff(data: &[u8]) -> (SideBytes, bool) {
    let binary = crate::merge_file::is_binary(data);
    (SideBytes::from_bytes(data.to_vec()), binary)
}

fn mode_display(mode: &str) -> Option<String> {
    if mode == "000000" {
        None
    } else {
        Some(mode.to_owned())
    }
}

fn materialize_no_index_oids(odb: &Odb, entries: &mut [DiffEntry]) -> Result<(), NoIndexError> {
    for entry in entries {
        if entry.old_mode != "000000" && entry.old_oid.is_zero() {
            if let Some(path) = entry.old_path.as_deref() {
                entry.old_oid = hash_side_at_path(odb, path, &entry.old_mode)?;
            }
        }
        if entry.new_mode != "000000" && entry.new_oid.is_zero() {
            if let Some(path) = entry.new_path.as_deref() {
                entry.new_oid = hash_side_at_path(odb, path, &entry.new_mode)?;
            }
        }
    }
    Ok(())
}

fn hash_side_at_path(odb: &Odb, path: &str, mode: &str) -> Result<ObjectId, NoIndexError> {
    let mode_u = u32::from_str_radix(mode, 8).unwrap_or(MODE_REGULAR);
    let data = read_worktree_bytes_for_mode(Path::new(path), mode_u).map_err(|_| {
        NoIndexError::PathNotAccessible {
            path: path.to_owned(),
        }
    })?;
    let oid = odb.hash(ObjectKind::Blob, &data);
    let _ = odb.write(ObjectKind::Blob, &data);
    Ok(oid)
}

fn fixup_paths(a: &Path, b: &Path) -> Result<(Option<PathBuf>, Option<PathBuf>), NoIndexError> {
    let meta_a = fs::symlink_metadata(a).ok();
    let meta_b = fs::symlink_metadata(b).ok();
    if meta_a.is_none() && meta_b.is_none() {
        return Err(NoIndexError::PathNotAccessible {
            path: a.display().to_string(),
        });
    }
    let is_dir_a = meta_a.as_ref().is_some_and(|m| m.is_dir());
    let is_dir_b = meta_b.as_ref().is_some_and(|m| m.is_dir());
    if is_dir_a && is_dir_b {
        return Ok((Some(a.to_path_buf()), Some(b.to_path_buf())));
    }
    if is_dir_a {
        let mut joined = a.to_path_buf();
        append_basename(&mut joined, b);
        return Ok((Some(joined), Some(b.to_path_buf())));
    }
    if is_dir_b {
        let mut joined = b.to_path_buf();
        append_basename(&mut joined, a);
        return Ok((Some(a.to_path_buf()), Some(joined)));
    }
    Ok((Some(a.to_path_buf()), Some(b.to_path_buf())))
}

fn append_basename(dir: &mut PathBuf, file: &Path) {
    let name = file.file_name().map(|s| s.to_owned()).unwrap_or_default();
    if let Some(stripped) = dir.as_os_str().to_str().filter(|s| s.ends_with('/')) {
        let _ = stripped;
    }
    while dir.as_os_str().to_string_lossy().ends_with('/') {
        dir.pop();
    }
    dir.push(name);
}

fn queue_diff(
    left: Option<&Path>,
    right: Option<&Path>,
    recursing: bool,
    out: &mut Vec<DiffEntry>,
) -> Result<(), NoIndexError> {
    let (mode_left, mode_right) = (path_mode(left, recursing)?, path_mode(right, recursing)?);
    let dir_left = path_is_dir(left);
    let dir_right = path_is_dir(right);

    if mode_left != 0 && mode_right != 0 && dir_left != dir_right {
        if dir_left {
            emit_pair(
                out,
                None,
                right,
                0,
                mode_right,
                right.map(path_display_string),
            );
            return queue_diff(None, right, true, out);
        }
        emit_pair(out, left, None, mode_left, 0, left.map(path_display_string));
        return queue_diff(left, None, true, out);
    }

    if dir_left || dir_right {
        diff_directories(left, right, out)
    } else {
        emit_pair(
            out,
            left,
            right,
            mode_left,
            mode_right,
            left.or(right).map(path_display_string),
        );
        Ok(())
    }
}

fn path_is_dir(path: Option<&Path>) -> bool {
    path.and_then(|p| fs::symlink_metadata(p).ok())
        .is_some_and(|m| m.is_dir())
}

fn diff_directories(
    left: Option<&Path>,
    right: Option<&Path>,
    out: &mut Vec<DiffEntry>,
) -> Result<(), NoIndexError> {
    let left_names = read_dir_names(left)?;
    let right_names = read_dir_names(right)?;
    let names: BTreeSet<_> = left_names
        .iter()
        .chain(right_names.iter())
        .cloned()
        .collect();
    for name in names {
        let l = left.map(|p| p.join(&name));
        let r = right.map(|p| p.join(&name));
        queue_diff(l.as_deref(), r.as_deref(), true, out)?;
    }
    Ok(())
}

fn read_dir_names(path: Option<&Path>) -> Result<Vec<String>, NoIndexError> {
    let Some(path) = path else {
        return Ok(Vec::new());
    };
    let mut names = Vec::new();
    let read_dir = fs::read_dir(path).map_err(|_| NoIndexError::PathNotAccessible {
        path: path.display().to_string(),
    })?;
    for entry in read_dir {
        let entry = entry.map_err(|_| NoIndexError::PathNotAccessible {
            path: path.display().to_string(),
        })?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." {
            continue;
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn emit_pair(
    out: &mut Vec<DiffEntry>,
    left: Option<&Path>,
    right: Option<&Path>,
    mode_left: u32,
    mode_right: u32,
    display: Option<String>,
) {
    let (status, old_path, new_path) = match (left, right) {
        (Some(_l), Some(_r)) if mode_left == 0 || mode_right == 0 => {
            if mode_left == 0 {
                (DiffStatus::Added, None, display.clone())
            } else {
                (DiffStatus::Deleted, display.clone(), None)
            }
        }
        (Some(l), None) => (DiffStatus::Deleted, Some(path_display_string(l)), None),
        (None, Some(r)) => (DiffStatus::Added, None, Some(path_display_string(r))),
        (Some(l), Some(r)) => {
            let lp = path_display_string(l);
            let rp = path_display_string(r);
            let status = if mode_left != mode_right {
                DiffStatus::TypeChanged
            } else {
                DiffStatus::Modified
            };
            (status, Some(lp), Some(rp))
        }
        (None, None) => return,
    };

    let old_mode = if mode_left == 0 {
        "000000".to_owned()
    } else {
        format_mode(mode_left)
    };
    let new_mode = if mode_right == 0 {
        "000000".to_owned()
    } else {
        format_mode(mode_right)
    };

    let old_oid = zero_oid();
    let new_oid = zero_oid();

    out.push(DiffEntry {
        status,
        old_path,
        new_path,
        old_mode,
        new_mode,
        old_oid,
        new_oid,
        score: None,
    });
}

fn path_mode(path: Option<&Path>, recursing: bool) -> Result<u32, NoIndexError> {
    let Some(path) = path else {
        return Ok(0);
    };
    if !recursing && path.as_os_str().is_empty() {
        return Ok(0);
    }
    let meta = fs::symlink_metadata(path).map_err(|_| NoIndexError::PathNotAccessible {
        path: path.display().to_string(),
    })?;
    Ok(mode_from_metadata(&meta))
}

fn path_display_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
