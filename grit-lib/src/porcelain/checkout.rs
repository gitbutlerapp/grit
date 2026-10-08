//! `git checkout` worktree-apply primitives.
//!
//! `checkout` is a large worktree mutator. Its CLI shell
//! (`grit/src/commands/checkout.rs`) still owns argv/clap parsing,
//! branch-switch messaging, hook dispatch, progress-to-stderr, and the
//! detached-HEAD advice text. This module holds the **pure worktree-apply
//! primitives** that shell calls into: writing a blob's bytes to a working-tree
//! path (handling symlinks, executable bits, and parent-directory
//! preparation), removing now-empty parent directories, and the simple glob
//! matcher used to resolve interactive-patch path filters.
//!
//! These functions compute and apply worktree changes from index/object data
//! and make no presentation decisions — no colour, pager, tty, or stdout.

use std::collections::{BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::diff::{diff_trees, DiffEntry, DiffStatus};
use crate::error::{Error, Result};
use crate::index::{entry_from_metadata, IndexEntry, MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK};
use crate::objects::ObjectId;
use crate::repo::Repository;

/// Apply the change from tree `from` to tree `to` onto the working tree and
/// index, then write the index.
///
/// This is the reusable core of a clean branch switch / fast-forward: it writes,
/// updates, and deletes only the paths that differ between the two trees, and
/// rebuilds the matching index entries (with fresh stat data so a subsequent
/// status reports a clean tree). Unchanged paths are left untouched.
///
/// The caller is responsible for policy: this function assumes the tracked
/// working tree is **clean** relative to `from` (no staged or unstaged changes
/// that would be silently overwritten) and that no untracked file sits where an
/// added path needs to land. It performs the mechanical apply only.
pub fn checkout_between_trees(
    repo: &Repository,
    from: Option<&ObjectId>,
    to: &ObjectId,
) -> Result<()> {
    let changes = diff_trees(&repo.odb, from, Some(to), "")?;
    checkout_tree_changes(repo, &changes)
}

/// Apply a precomputed tree-to-tree diff to the working tree and index.
///
/// Callers such as `grit switch` can reuse one [`diff_trees`] result for both
/// untracked guards and the mechanical apply.
pub fn checkout_tree_changes(repo: &Repository, changes: &[DiffEntry]) -> Result<()> {
    let work_tree = repo
        .work_tree
        .clone()
        .ok_or_else(|| Error::PathError("cannot update a bare repository's working tree".into()))?;

    let mut index = repo.load_index()?;
    let mut paths_to_remove: Vec<Vec<u8>> = Vec::with_capacity(changes.len() / 8);
    let mut new_entries: Vec<IndexEntry> = Vec::with_capacity(changes.len());
    let mut deleted_abs_paths: Vec<PathBuf> = Vec::with_capacity(changes.len() / 8);
    let mut dir_cache = LeadingDirCache::new();

    for change in changes {
        if change.status == DiffStatus::Deleted {
            if let Some(path) = &change.old_path {
                let abs = work_tree.join(path);
                let _ = fs::remove_file(&abs);
                deleted_abs_paths.push(abs);
                paths_to_remove.push(path.as_bytes().to_vec());
            }
            continue;
        }

        let Some(path) = &change.new_path else {
            continue;
        };
        let mode = parse_git_mode(&change.new_mode);
        let object = repo.odb.read(&change.new_oid)?;
        let entry = write_checkout_entry(
            &work_tree,
            path,
            &object.data,
            mode,
            change,
            &mut dir_cache,
            change.new_oid,
        )?;
        new_entries.push(entry);
    }

    remove_empty_parent_dirs_batch(&work_tree, &deleted_abs_paths);

    index.remove_paths_and_insert(paths_to_remove.iter().map(|p| p.as_slice()), new_entries);
    repo.write_index(&mut index)?;
    Ok(())
}

fn parse_git_mode(mode: &str) -> u32 {
    u32::from_str_radix(mode, 8).unwrap_or(MODE_REGULAR)
}

fn is_regular_git_mode(mode: u32) -> bool {
    mode == MODE_REGULAR || mode == MODE_EXECUTABLE
}

fn can_overwrite_regular_in_place(change: &DiffEntry) -> bool {
    change.status == DiffStatus::Modified
        && is_regular_git_mode(parse_git_mode(&change.old_mode))
        && is_regular_git_mode(parse_git_mode(&change.new_mode))
}

fn write_checkout_entry(
    work_tree: &Path,
    rel_path: &str,
    data: &[u8],
    mode: u32,
    change: &DiffEntry,
    dir_cache: &mut LeadingDirCache,
    oid: ObjectId,
) -> Result<IndexEntry> {
    if mode == MODE_SYMLINK {
        write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
        let abs = work_tree.join(rel_path);
        let meta = written_file_symlink_metadata(&abs)?;
        return Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode));
    }

    if can_overwrite_regular_in_place(change) {
        dir_cache.ensure_parents(work_tree, rel_path)?;
        let abs_path = work_tree.join(rel_path);
        let old_mode = parse_git_mode(&change.old_mode);
        let mut file = match OpenOptions::new()
            .write(true)
            .truncate(true)
            .open(&abs_path)
        {
            Ok(f) => f,
            Err(_) => {
                write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
                let meta = written_file_symlink_metadata(&abs_path)?;
                return Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode));
            }
        };
        file.write_all(data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}' during checkout: {e}")))?;
        let meta = written_file_metadata(&file, &abs_path)?;
        apply_in_place_git_mode_transition(&file, &meta, old_mode, mode)?;
        return Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode));
    }

    write_to_worktree_cached(work_tree, rel_path, data, mode, dir_cache)?;
    let abs = work_tree.join(rel_path);
    let meta = written_file_symlink_metadata(&abs)?;
    Ok(entry_from_metadata(&meta, rel_path.as_bytes(), oid, mode))
}

/// Adjust only the executable bit for an in-place overwrite, preserving umask-driven permissions.
#[cfg(unix)]
fn apply_in_place_git_mode_transition(
    file: &std::fs::File,
    meta: &fs::Metadata,
    old_mode: u32,
    new_mode: u32,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let old_exec = old_mode == MODE_EXECUTABLE;
    let new_exec = new_mode == MODE_EXECUTABLE;
    if old_exec == new_exec {
        return Ok(());
    }
    let mut perms = meta.permissions();
    let mut bits = perms.mode();
    const ALL_EXEC: u32 = 0o111;
    if new_exec {
        // Match Git: grant execute wherever read is already present (0644→0755 under umask 022,
        // 0600→0700 under umask 077).
        bits |= (bits & 0o444) >> 2;
    } else {
        bits &= !ALL_EXEC;
    }
    perms.set_mode(bits);
    file.set_permissions(perms).map_err(Error::Io)
}

#[cfg(not(unix))]
fn apply_in_place_git_mode_transition(
    _file: &std::fs::File,
    _meta: &fs::Metadata,
    _old_mode: u32,
    _new_mode: u32,
) -> Result<()> {
    Ok(())
}

/// Tracks leading directories already verified or created during one checkout pass.
struct LeadingDirCache {
    ready: HashSet<PathBuf>,
}

impl LeadingDirCache {
    fn new() -> Self {
        Self {
            ready: HashSet::new(),
        }
    }

    fn ensure_parents(&mut self, work_tree: &Path, rel_path: &str) -> Result<()> {
        use std::path::Component;
        let path = Path::new(rel_path);
        let Some(parent_rel) = path.parent() else {
            return Ok(());
        };
        if parent_rel.as_os_str().is_empty() {
            return Ok(());
        }
        let mut cur = work_tree.to_path_buf();
        for comp in parent_rel.components() {
            if let Component::Normal(name) = comp {
                cur.push(name);
                if self.ready.contains(&cur) {
                    continue;
                }
                if let Ok(meta) = leading_dir_metadata(&cur) {
                    if meta.file_type().is_symlink() || !meta.is_dir() {
                        fs::remove_file(&cur)?;
                    }
                }
                if fs::create_dir(&cur).is_err() {
                    // Exists as a directory already.
                }
                self.ready.insert(cur.clone());
            }
        }
        Ok(())
    }
}

fn leading_dir_metadata(path: &Path) -> Result<fs::Metadata> {
    #[cfg(test)]
    syscall_probe::note_metadata(path, syscall_probe::MetadataKind::Directory);
    fs::symlink_metadata(path).map_err(Error::Io)
}

fn written_file_symlink_metadata(path: &Path) -> Result<fs::Metadata> {
    #[cfg(test)]
    syscall_probe::note_metadata(path, syscall_probe::MetadataKind::WrittenFile);
    fs::symlink_metadata(path).map_err(Error::Io)
}

fn written_file_metadata(file: &std::fs::File, path: &Path) -> Result<fs::Metadata> {
    #[cfg(test)]
    {
        syscall_probe::note_metadata(path, syscall_probe::MetadataKind::WrittenFile);
    }
    #[cfg(not(test))]
    let _ = path;
    file.metadata().map_err(Error::Io)
}

/// Set `abs_path` permissions to match Git index `mode` (regular vs executable blob).
pub fn apply_index_file_mode(abs_path: &Path, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if mode != MODE_EXECUTABLE {
            return Ok(());
        }
        let mut perms = fs::metadata(abs_path)?.permissions();
        let mut bits = perms.mode();
        bits |= 0o100;
        perms.set_mode(bits);
        fs::set_permissions(abs_path, perms)?;
    }
    // Windows has no POSIX mode bits; the executable bit is not represented in
    // the filesystem, so this is a no-op there.
    #[cfg(not(unix))]
    let _ = (abs_path, mode);
    Ok(())
}

/// Ensure each component of `rel_path`'s parent exists as a real directory.
///
/// Replaces a parent path that is a symlink or regular file (e.g. `D` → `untracked` or `D` as a
/// file) so `mkdir -p` can create `D/A` during checkout (`t2080` force checkout cases).
pub fn prepare_parent_dirs_for_checkout(work_tree: &Path, rel_path: &str) -> Result<()> {
    let mut cache = LeadingDirCache::new();
    cache.ensure_parents(work_tree, rel_path)?;
    let abs_path = work_tree.join(rel_path);
    if let Some(parent) = abs_path.parent() {
        fs::create_dir_all(parent).map_err(|e| {
            Error::PathError(format!("creating parent directories for '{rel_path}': {e}"))
        })?;
    }
    Ok(())
}

/// Write data to a working tree file, handling symlinks and executable bits.
pub fn write_to_worktree(work_tree: &Path, rel_path: &str, data: &[u8], mode: u32) -> Result<()> {
    write_to_worktree_cached(work_tree, rel_path, data, mode, &mut LeadingDirCache::new())
}

fn write_to_worktree_cached(
    work_tree: &Path,
    rel_path: &str,
    data: &[u8],
    mode: u32,
    dir_cache: &mut LeadingDirCache,
) -> Result<()> {
    let abs_path = work_tree.join(rel_path);

    dir_cache.ensure_parents(work_tree, rel_path)?;
    if let Some(parent) = abs_path.parent() {
        if !dir_cache.ready.contains(parent) {
            fs::create_dir_all(parent).map_err(|e| {
                Error::PathError(format!("creating parent directories for '{rel_path}': {e}"))
            })?;
            dir_cache.ready.insert(parent.to_path_buf());
        }
    }

    // Remove existing file/dir/symlink at target path. Use symlink_metadata + is_symlink so we
    // replace symlinked paths (e.g. `D` → `untracked`) before creating a real directory tree.
    if let Ok(meta) = fs::symlink_metadata(&abs_path) {
        if meta.file_type().is_symlink() {
            fs::remove_file(&abs_path)?;
        } else if meta.is_dir() {
            fs::remove_dir_all(&abs_path)?;
        } else {
            fs::remove_file(&abs_path)?;
        }
    }

    if mode == MODE_SYMLINK {
        let target = std::str::from_utf8(data).map_err(|_| {
            Error::PathError(format!("symlink target for '{rel_path}' is not UTF-8"))
        })?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &abs_path)
            .map_err(|e| Error::PathError(format!("creating symlink '{rel_path}': {e}")))?;
        // Windows lacks unprivileged symlink creation; materialise the link as a
        // regular file containing the target path so the worktree stays populated.
        #[cfg(not(unix))]
        fs::write(&abs_path, target.as_bytes())
            .map_err(|e| Error::PathError(format!("writing symlink '{rel_path}': {e}")))?;
    } else {
        fs::write(&abs_path, data)
            .map_err(|e| Error::PathError(format!("writing '{rel_path}': {e}")))?;

        if mode == MODE_EXECUTABLE {
            apply_index_file_mode(&abs_path, MODE_EXECUTABLE)?;
        }
    }

    Ok(())
}

/// Remove empty parent directories up to (but not including) `work_tree`.
pub fn remove_empty_parent_dirs(work_tree: &Path, path: &Path) {
    remove_empty_parent_dirs_batch(work_tree, &[path.to_path_buf()]);
}

fn remove_empty_parent_dirs_batch(work_tree: &Path, deleted_paths: &[PathBuf]) {
    let cwd = std::env::current_dir().ok();
    let mut dirs: BTreeSet<PathBuf> = BTreeSet::new();
    for path in deleted_paths {
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == work_tree {
                break;
            }
            dirs.insert(dir.to_path_buf());
            current = dir.parent();
        }
    }
    let mut ordered: Vec<PathBuf> = dirs.into_iter().collect();
    ordered.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for dir in ordered {
        if cwd
            .as_ref()
            .is_some_and(|cwd| cwd == &dir || cwd.starts_with(&dir))
        {
            continue;
        }
        let _ = fs::remove_dir(&dir);
    }
}

/// Check if a pathspec contains glob characters.
pub fn is_glob_pattern(spec: &str) -> bool {
    spec.contains('*') || spec.contains('?') || spec.contains('[')
}

/// Match a path against a simple glob pattern.
/// Supports `*` (any chars except `/`), `?` (any single char except `/`),
/// and character classes `[abc]`.
pub fn glob_matches(pattern: &str, path: &str) -> bool {
    glob_matches_inner(pattern.as_bytes(), path.as_bytes())
}

fn glob_matches_inner(pattern: &[u8], path: &[u8]) -> bool {
    let mut pi = 0; // pattern index
    let mut si = 0; // string index
    let mut star_pi = usize::MAX;
    let mut star_si = 0;

    while si < path.len() {
        if pi < pattern.len() && pattern[pi] == b'?' {
            pi += 1;
            si += 1;
        } else if pi < pattern.len() && pattern[pi] == b'*' {
            if pi + 1 < pattern.len() && pattern[pi + 1] == b'*' {
                // "**" matches everything including '/'
                // For simplicity, try matching rest of pattern at every position
                let rest = &pattern[pi + 2..];
                // Skip optional '/' after **
                let rest = if !rest.is_empty() && rest[0] == b'/' {
                    &rest[1..]
                } else {
                    rest
                };
                for i in si..=path.len() {
                    if glob_matches_inner(rest, &path[i..]) {
                        return true;
                    }
                }
                return false;
            }
            star_pi = pi;
            star_si = si;
            pi += 1;
        } else if pi < pattern.len() && pattern[pi] == b'[' {
            // Character class
            pi += 1;
            let negate = pi < pattern.len() && (pattern[pi] == b'!' || pattern[pi] == b'^');
            if negate {
                pi += 1;
            }
            let mut found = false;
            let ch = path[si];
            while pi < pattern.len() && pattern[pi] != b']' {
                if pi + 2 < pattern.len() && pattern[pi + 1] == b'-' {
                    if ch >= pattern[pi] && ch <= pattern[pi + 2] {
                        found = true;
                    }
                    pi += 3;
                } else {
                    if ch == pattern[pi] {
                        found = true;
                    }
                    pi += 1;
                }
            }
            if pi < pattern.len() {
                pi += 1;
            } // skip ']'
            if found == negate {
                // Mismatch in character class
                if star_pi != usize::MAX {
                    pi = star_pi + 1;
                    star_si += 1;
                    si = star_si;
                } else {
                    return false;
                }
            } else {
                si += 1;
            }
        } else if pi < pattern.len() && pattern[pi] == path[si] {
            pi += 1;
            si += 1;
        } else if star_pi != usize::MAX {
            // Backtrack: '*' matches one more character (including '/')
            pi = star_pi + 1;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }

    // Consume trailing '*' or '**' in pattern
    while pi < pattern.len() && pattern[pi] == b'*' {
        pi += 1;
    }

    pi == pattern.len()
}

#[cfg(test)]
mod syscall_probe {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};

    #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
    pub enum MetadataKind {
        Directory,
        WrittenFile,
    }

    thread_local! {
        static COUNTS: RefCell<HashMap<(PathBuf, MetadataKind), u32>> =
            RefCell::new(HashMap::new());
    }

    pub fn reset() {
        COUNTS.with(|c| c.borrow_mut().clear());
    }

    pub fn note_metadata(path: &Path, kind: MetadataKind) {
        COUNTS.with(|c| {
            let mut map = c.borrow_mut();
            *map.entry((path.to_path_buf(), kind)).or_insert(0) += 1;
        });
    }

    pub fn max_per_directory() -> u32 {
        COUNTS.with(|c| {
            c.borrow()
                .iter()
                .filter(|((_, k), _)| *k == MetadataKind::Directory)
                .map(|(_, v)| *v)
                .max()
                .unwrap_or(0)
        })
    }

    pub fn max_per_written_file() -> u32 {
        COUNTS.with(|c| {
            c.borrow()
                .iter()
                .filter(|((_, k), _)| *k == MetadataKind::WrittenFile)
                .map(|(_, v)| *v)
                .max()
                .unwrap_or(0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::entry_from_stat;
    use crate::objects::ObjectKind;
    use crate::repo::init_repository;
    use crate::write_tree::{write_tree_update_index, WriteTreeFlags};
    use tempfile::TempDir;

    fn commit_all(repo: &Repository, message: &str) -> ObjectId {
        use crate::objects::{serialize_commit, CommitData};
        use crate::refs;
        let work_tree = repo.work_tree.as_deref().expect("work tree");
        let mut index = repo.load_index().expect("index");
        for entry in std::fs::read_dir(work_tree).expect("read_dir") {
            let entry = entry.expect("entry");
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let rel = path.strip_prefix(work_tree).expect("rel").to_string_lossy();
            let data = fs::read(&path).expect("read");
            let oid = repo.odb.write(ObjectKind::Blob, &data).expect("blob");
            let e = entry_from_stat(&path, rel.as_bytes(), oid, MODE_REGULAR).expect("entry");
            index.add_or_replace(e);
        }
        index.sort();
        let tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())
            .expect("tree");
        repo.write_index(&mut index).expect("write index");
        let parent = refs::resolve_ref(&repo.git_dir, "HEAD").ok();
        let commit_data = CommitData {
            tree,
            parents: parent.into_iter().collect(),
            author: "Test <t@example.com>".to_owned(),
            committer: "Test <t@example.com>".to_owned(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("{message}\n"),
            raw_message: None,
        };
        let bytes = serialize_commit(&commit_data);
        let commit_oid = repo.odb.write(ObjectKind::Commit, &bytes).expect("commit");
        refs::write_ref(&repo.git_dir, "HEAD", &commit_oid).expect("head");
        tree
    }

    #[test]
    fn checkout_syscall_counts_one_metadata_per_dir_and_file() {
        syscall_probe::reset();
        let tmp = TempDir::new().expect("tempdir");
        let repo = init_repository(tmp.path(), false, "main", None, "files").expect("init");
        let wt = repo.work_tree.as_ref().expect("wt");
        for d in 0..5 {
            let dir = wt.join(format!("dir{d}"));
            fs::create_dir_all(&dir).expect("mkdir");
            for f in 0..20 {
                fs::write(dir.join(format!("f{f}.txt")), format!("body {d}-{f}\n")).expect("w");
            }
        }
        let head = commit_all(&repo, "initial");
        for d in 0..5 {
            let dir = wt.join(format!("dir{d}"));
            for f in 0..20 {
                fs::write(dir.join(format!("f{f}.txt")), format!("new {d}-{f}\n")).expect("w");
            }
        }
        let mut index = repo.load_index().expect("index");
        for d in 0..5 {
            for f in 0..20 {
                let rel = format!("dir{d}/f{f}.txt");
                let abs = wt.join(&rel);
                let data = fs::read(&abs).expect("read");
                let oid = repo.odb.write(ObjectKind::Blob, &data).expect("blob");
                let entry = entry_from_stat(&abs, rel.as_bytes(), oid, MODE_REGULAR).expect("e");
                index.add_or_replace(entry);
            }
        }
        index.sort();
        let to_tree = write_tree_update_index(&repo.odb, &mut index, "", WriteTreeFlags::silent())
            .expect("t");

        syscall_probe::reset();
        checkout_between_trees(&repo, Some(&head), &to_tree).expect("checkout");

        assert!(
            syscall_probe::max_per_directory() <= 1,
            "each directory should incur at most one tracked metadata call"
        );
        assert!(
            syscall_probe::max_per_written_file() <= 1,
            "each written file should incur at most one post-write metadata call"
        );
    }
}
