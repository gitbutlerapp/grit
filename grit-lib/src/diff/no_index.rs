//! Compare arbitrary filesystem paths without a repository (`git diff --no-index`).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::config::ConfigSet;
use crate::hash::hash_object;
use crate::index::MODE_REGULAR;
use crate::objects::{HashAlgo, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::parallel::ThreadPolicy;

use super::core::{
    apply_rename_detection_to_diff_entries, format_mode, mode_from_metadata,
    read_worktree_bytes_for_mode, zero_oid, DiffEntry, DiffStatus,
};
use super::options::DiffOptions;
use super::structured::{structured_file_diff, SideBytes};
use super::targets::FileDiff;

const S_IFMT: u32 = 0o170000;
const S_IFREG: u32 = 0o100000;

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
    /// SHA-1 collision detection rejected blob content while hashing a path.
    #[error("sha-1 hash collision while hashing no-index diff input")]
    HashCollision,
}

impl From<NoIndexError> for crate::error::Error {
    fn from(value: NoIndexError) -> Self {
        Self::Message(value.to_string())
    }
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
    store_changed_blobs(&odb, &mut entries)?;
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

    let diff_options = opts
        .clone()
        .cap_word_level_inline_for_changed_files(entries.len());
    Ok(entries
        .into_iter()
        .filter_map(|entry| file_diff_from_entry(&odb, &entry, diff_options.clone()))
        .collect())
}

fn file_diff_from_entry(odb: &Odb, entry: &DiffEntry, opts: DiffOptions) -> Option<FileDiff> {
    let path_unchanged = entry.old_path == entry.new_path;
    if entry.old_oid == entry.new_oid
        && entry.old_mode == entry.new_mode
        && path_unchanged
        && !matches!(entry.status, DiffStatus::Renamed | DiffStatus::Copied)
    {
        return None;
    }
    let (old_bytes, old_bin) = side_bytes_for_entry(odb, entry, Side::Old)?;
    let (new_bytes, new_bin) = side_bytes_for_entry(odb, entry, Side::New)?;
    let binary = old_bin || new_bin;
    let path = entry
        .new_path
        .clone()
        .or_else(|| entry.old_path.clone())
        .unwrap_or_default();
    let old_path = entry
        .old_path
        .clone()
        .filter(|op| entry.new_path.as_ref() != Some(op));
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

/// Write blob objects for changed sides only (rename detection reads from the ODB).
fn store_changed_blobs(odb: &Odb, entries: &mut [DiffEntry]) -> Result<(), NoIndexError> {
    for entry in entries {
        if entry.old_mode != "000000" && !entry.old_oid.is_zero() {
            if let Some(path) = entry.old_path.as_deref() {
                write_blob_if_missing(odb, path, &entry.old_mode, &mut entry.old_oid)?;
            }
        }
        if entry.new_mode != "000000" && !entry.new_oid.is_zero() {
            if let Some(path) = entry.new_path.as_deref() {
                write_blob_if_missing(odb, path, &entry.new_mode, &mut entry.new_oid)?;
            }
        }
    }
    Ok(())
}

fn write_blob_if_missing(
    odb: &Odb,
    path: &str,
    mode: &str,
    oid: &mut ObjectId,
) -> Result<(), NoIndexError> {
    if !oid.is_zero() && odb.read(oid).is_ok() {
        return Ok(());
    }
    *oid = hash_side_at_path(path, mode)?;
    let mode_u = u32::from_str_radix(mode, 8).unwrap_or(MODE_REGULAR);
    let data = read_worktree_bytes_for_mode(Path::new(path), mode_u).map_err(|_| {
        NoIndexError::PathNotAccessible {
            path: path.to_owned(),
        }
    })?;
    let _ = odb.write(ObjectKind::Blob, &data);
    Ok(())
}

fn hash_side_at_path(path: &str, mode: &str) -> Result<ObjectId, NoIndexError> {
    let mode_u = u32::from_str_radix(mode, 8).unwrap_or(MODE_REGULAR);
    let data = read_worktree_bytes_for_mode(Path::new(path), mode_u).map_err(|_| {
        NoIndexError::PathNotAccessible {
            path: path.to_owned(),
        }
    })?;
    hash_blob(&data)
}

fn hash_blob(data: &[u8]) -> Result<ObjectId, NoIndexError> {
    hash_object(HashAlgo::Sha1, ObjectKind::Blob, data).map_err(|e| match e {
        crate::error::Error::HashCollision { .. } => NoIndexError::HashCollision,
        other => panic!("unexpected error hashing no-index blob: {other}"),
    })
}

fn read_side_bytes(path: &Path, mode: u32) -> Result<Vec<u8>, NoIndexError> {
    read_worktree_bytes_for_mode(path, mode).map_err(|_| NoIndexError::PathNotAccessible {
        path: path.display().to_string(),
    })
}

fn worktree_content_equal(
    left: &Path,
    right: &Path,
    mode_left: u32,
    mode_right: u32,
) -> Result<bool, NoIndexError> {
    let (ls, rs) = (
        fs::metadata(left).map(|m| m.len()).ok(),
        fs::metadata(right).map(|m| m.len()).ok(),
    );
    if ls != rs {
        return Ok(false);
    }
    let old_bytes = read_side_bytes(left, mode_left)?;
    let new_bytes = read_side_bytes(right, mode_right)?;
    Ok(old_bytes == new_bytes)
}

/// Blob OIDs for a path pair, reading each present side at most once.
fn oids_for_pair(
    left: Option<&Path>,
    right: Option<&Path>,
    mode_left: u32,
    mode_right: u32,
) -> Result<(ObjectId, ObjectId), NoIndexError> {
    match (left, right) {
        (Some(l), Some(r)) if mode_left != 0 && mode_right != 0 => {
            let old_bytes = read_side_bytes(l, mode_left)?;
            let new_bytes = read_side_bytes(r, mode_right)?;
            let old_oid = hash_blob(&old_bytes)?;
            let new_oid = if old_bytes == new_bytes {
                old_oid
            } else {
                hash_blob(&new_bytes)?
            };
            Ok((old_oid, new_oid))
        }
        (Some(l), _) if mode_left != 0 => {
            let old_bytes = read_side_bytes(l, mode_left)?;
            Ok((hash_blob(&old_bytes)?, zero_oid()))
        }
        (_, Some(r)) if mode_right != 0 => {
            let new_bytes = read_side_bytes(r, mode_right)?;
            Ok((zero_oid(), hash_blob(&new_bytes)?))
        }
        _ => Ok((zero_oid(), zero_oid())),
    }
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
    let name = file.file_name().unwrap_or_default();
    if name.is_empty() {
        return;
    }
    strip_trailing_slashes_preserve_root(dir);
    dir.push(name);
}

/// Remove trailing `/` without turning `/` into an empty relative path.
fn strip_trailing_slashes_preserve_root(path: &mut PathBuf) {
    let raw = path.as_os_str().to_string_lossy();
    let trimmed = raw.trim_end_matches('/');
    if trimmed.is_empty() {
        *path = PathBuf::from("/");
    } else {
        *path = PathBuf::from(trimmed);
    }
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
            )?;
            return queue_diff(left, None, true, out);
        }
        emit_pair(out, left, None, mode_left, 0, left.map(path_display_string))?;
        return queue_diff(None, right, true, out);
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
        )?;
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
    let left_set: HashSet<_> = left_names.iter().cloned().collect();
    let right_set: HashSet<_> = right_names.iter().cloned().collect();
    let names: BTreeSet<_> = left_names
        .iter()
        .chain(right_names.iter())
        .cloned()
        .collect();
    for name in names {
        let l = left
            .filter(|_| left_set.contains(&name))
            .map(|p| p.join(&name));
        let r = right
            .filter(|_| right_set.contains(&name))
            .map(|p| p.join(&name));
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
) -> Result<(), NoIndexError> {
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
            let status = if file_kind(mode_left) != file_kind(mode_right) {
                DiffStatus::TypeChanged
            } else {
                DiffStatus::Modified
            };
            (status, Some(lp), Some(rp))
        }
        (None, None) => return Ok(()),
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

    if let (Some(l), Some(r)) = (left, right) {
        if mode_left != 0
            && mode_right != 0
            && old_mode == new_mode
            && worktree_content_equal(l, r, mode_left, mode_right)?
        {
            return Ok(());
        }
    }

    let (old_oid, new_oid) = oids_for_pair(left, right, mode_left, mode_right)?;

    if old_oid == new_oid && old_mode == new_mode {
        return Ok(());
    }

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
    Ok(())
}

fn file_kind(mode: u32) -> u32 {
    if mode & S_IFMT != 0 {
        mode & S_IFMT
    } else {
        S_IFREG
    }
}

fn path_mode(path: Option<&Path>, recursing: bool) -> Result<u32, NoIndexError> {
    let Some(path) = path else {
        return Ok(0);
    };
    if !recursing && path.as_os_str().is_empty() {
        return Ok(0);
    }
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(mode_from_metadata(&meta)),
        Err(_) if recursing => Ok(0),
        Err(_) => Err(NoIndexError::PathNotAccessible {
            path: path.display().to_string(),
        }),
    }
}

fn path_display_string(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}
