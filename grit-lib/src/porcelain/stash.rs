//! `git stash apply` core, plus the tree-flattening and worktree-mutation
//! primitives the stash engine is built on.
//!
//! The library owns the *computation and worktree/index mutation* of applying a
//! stash commit onto the current worktree and index — the three-way merge when
//! HEAD has moved, conflict detection, and the index rebuild — while the `grit`
//! binary keeps argument parsing, the `Dropped …`/conflict messaging, and
//! exit-code mapping. [`apply_stash`] returns whether conflicts occurred so the
//! CLI can decide whether to drop the entry and what to print.
//!
//! The flattening/mutation helpers ([`FlatTreeEntry`], [`flatten_tree_full`],
//! [`add_stage_entry`], [`worktree_bytes_for_index_mode`],
//! [`write_regular_file_replacing_symlink`], [`remove_empty_dirs`]) are shared
//! with the still-CLI-resident stash-create/show paths, so they are public.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io;
use std::path::Path;

use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry, MODE_GITLINK, MODE_SYMLINK};
// `MODE_EXECUTABLE` only drives the Unix executable-bit application below.
#[cfg(unix)]
use crate::index::MODE_EXECUTABLE;
use crate::objects::{parse_commit, parse_tree, CommitData, ObjectId};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::state::resolve_head;

/// A single blob entry from a recursively flattened tree.
#[derive(Clone)]
pub struct FlatTreeEntry {
    pub path: String,
    pub mode: u32,
    pub oid: ObjectId,
}

/// Recursively flatten a tree into (path, mode, oid) entries.
pub fn flatten_tree_full(
    odb: &Odb,
    tree_oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<FlatTreeEntry>> {
    let obj = odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();
    for entry in entries {
        let entry_name = String::from_utf8_lossy(&entry.name).to_string();
        let full_path = if prefix.is_empty() {
            entry_name
        } else {
            format!("{prefix}/{entry_name}")
        };
        if entry.mode == 0o40000 {
            let sub = flatten_tree_full(odb, &entry.oid, &full_path)?;
            result.extend(sub);
        } else {
            result.push(FlatTreeEntry {
                path: full_path,
                mode: entry.mode,
                oid: entry.oid,
            });
        }
    }
    Ok(result)
}

/// Push a conflict (non-zero) stage entry for `path` into `index`.
pub fn add_stage_entry(index: &mut Index, path: &[u8], oid: &ObjectId, mode: u32, stage: u16) {
    let name_len = path.len().min(0xFFF) as u16;
    let flags = (stage << 12) | name_len;
    index.push_entry_unsorted(IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: *oid,
        flags,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    });
}

/// Read the worktree bytes for `path`, honoring a symlink index mode (returns
/// the link target rather than following it).
pub fn worktree_bytes_for_index_mode(path: &Path, mode: u32) -> io::Result<Vec<u8>> {
    if mode == MODE_SYMLINK {
        let target = fs::read_link(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStrExt;
            return Ok(target.as_os_str().as_bytes().to_vec());
        }
        #[cfg(not(unix))]
        {
            return Ok(target.to_string_lossy().as_bytes().to_vec());
        }
    }
    fs::read(path)
}

/// Write a regular file at `path`, first removing any pre-existing symlink there.
pub fn write_regular_file_replacing_symlink(path: &Path, contents: &[u8]) -> io::Result<()> {
    if path
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        fs::remove_file(path)?;
    }
    fs::write(path, contents)
}

/// Remove now-empty directories from `dir` upward toward (but not including)
/// `stop_at`, refusing to remove a directory that contains the process CWD.
pub fn remove_empty_dirs(dir: &Path, stop_at: &Path) {
    let cwd_rel = crate::worktree_cwd::process_cwd_repo_relative(stop_at);
    let mut current = dir.to_path_buf();
    while current != stop_at {
        if fs::read_dir(&current)
            .map(|mut d| d.next().is_none())
            .unwrap_or(false)
        {
            if let Some(ref cr) = cwd_rel {
                if crate::worktree_cwd::cwd_would_be_removed_with_dir(stop_at, &current, cr) {
                    break;
                }
            }
            let _ = fs::remove_dir(&current);
            if let Some(parent) = current.parent() {
                current = parent.to_path_buf();
            } else {
                break;
            }
        } else {
            break;
        }
    }
}

/// Paths whose worktree content the stash would change vs. its HEAD-at-stash base.
pub fn stash_worktree_change_paths(
    repo: &Repository,
    stash_commit: &CommitData,
) -> Result<BTreeSet<String>> {
    let head_at_stash = stash_commit.parents.first().ok_or_else(|| {
        Error::Message("corrupt stash commit: expected at least 2 parents".into())
    })?;
    let stash_tree_entries = flatten_tree_full(&repo.odb, &stash_commit.tree, "")?;
    let head_obj = repo.odb.read(head_at_stash)?;
    let head_commit = parse_commit(&head_obj.data)?;
    let base_tree_entries = flatten_tree_full(&repo.odb, &head_commit.tree, "")?;

    let base_map: BTreeMap<String, &FlatTreeEntry> = base_tree_entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let stash_map: BTreeMap<String, &FlatTreeEntry> = stash_tree_entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    let mut paths = BTreeSet::new();
    for (path, stash_entry) in &stash_map {
        match base_map.get(path) {
            Some(base_entry)
                if base_entry.oid != stash_entry.oid || base_entry.mode != stash_entry.mode =>
            {
                paths.insert(path.clone());
            }
            None => {
                paths.insert(path.clone());
            }
            _ => {}
        }
    }
    for path in base_map.keys() {
        if !stash_map.contains_key(path) {
            paths.insert(path.clone());
        }
    }
    Ok(paths)
}

/// Refuse to apply when the stash would clobber a locally-modified file.
pub fn check_stash_apply_would_overwrite_local_changes(
    repo: &Repository,
    work_tree: &Path,
    stash_commit: &CommitData,
) -> Result<()> {
    let current_index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };

    for path in stash_worktree_change_paths(repo, stash_commit)? {
        let file_path = work_tree.join(&path);
        let Some(idx_entry) = current_index.get(path.as_bytes(), 0) else {
            continue;
        };
        if idx_entry.mode == MODE_GITLINK {
            continue;
        }
        match worktree_bytes_for_index_mode(&file_path, idx_entry.mode) {
            Ok(contents) => {
                if let Ok(idx_blob) = repo.odb.read(&idx_entry.oid) {
                    if contents != idx_blob.data {
                        return Err(Error::Message(format!("error: Your local changes to the following files would be overwritten by merge:\n\t{path}\nPlease commit your changes or stash them before you merge.")));
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}

/// Apply a stash commit onto the current worktree and index.
///
/// Returns `true` if there were conflicts. Mirrors `git stash apply`:
/// three-way-merges changed files when HEAD has moved since the stash was
/// created, restores the index from the stash index parent when `restore_index`
/// is set (otherwise tracks current HEAD at touched paths), and materializes
/// any untracked-files parent. The CLI handles the `Dropped …`/conflict-kept
/// messaging around this.
pub fn apply_stash(
    repo: &Repository,
    work_tree: &Path,
    stash_oid: &ObjectId,
    restore_index: bool,
    _quiet: bool,
) -> Result<bool> {
    let obj = repo.odb.read(stash_oid)?;
    let stash_commit = parse_commit(&obj.data)?;

    if stash_commit.parents.len() < 2 {
        return Err(Error::Message(
            "corrupt stash commit: expected at least 2 parents".into(),
        ));
    }

    check_stash_apply_would_overwrite_local_changes(repo, work_tree, &stash_commit)?;

    let head_at_stash = &stash_commit.parents[0];
    let index_commit_oid = &stash_commit.parents[1];

    // Load current index
    let current_index = match repo.load_index() {
        Ok(idx) => idx,
        Err(Error::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => Index::new(),
        Err(e) => return Err(e),
    };

    // Read stash trees
    let stash_tree_entries = flatten_tree_full(&repo.odb, &stash_commit.tree, "")?;

    // Read HEAD-at-stash tree (base)
    let head_at_stash_obj = repo.odb.read(head_at_stash)?;
    let head_at_stash_commit = parse_commit(&head_at_stash_obj.data)?;
    let base_tree_entries = flatten_tree_full(&repo.odb, &head_at_stash_commit.tree, "")?;

    let base_map: BTreeMap<String, &FlatTreeEntry> = base_tree_entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();
    let stash_map: BTreeMap<String, &FlatTreeEntry> = stash_tree_entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    // Find files changed in the stash working tree vs base
    let mut wt_changes: BTreeMap<String, Option<&FlatTreeEntry>> = BTreeMap::new();
    for (path, stash_entry) in &stash_map {
        match base_map.get(path) {
            Some(base_entry)
                if base_entry.oid != stash_entry.oid || base_entry.mode != stash_entry.mode =>
            {
                wt_changes.insert(path.clone(), Some(stash_entry));
            }
            None => {
                wt_changes.insert(path.clone(), Some(stash_entry));
            }
            _ => {}
        }
    }
    // Track deletions (in base but not in stash)
    for path in base_map.keys() {
        if !stash_map.contains_key(path) {
            wt_changes.insert(path.clone(), None); // None = deleted
        }
    }

    // Check for conflicts: does the worktree have local modifications to files
    // that the stash also wants to change?
    for path in wt_changes.keys() {
        let file_path = work_tree.join(path);
        // Get the current index entry for this file
        if let Some(idx_entry) = current_index.get(path.as_bytes(), 0) {
            if idx_entry.mode == MODE_GITLINK {
                // Submodule: comparing index blob in the superproject ODB is wrong; t7402 expects
                // stash apply to succeed while the nested repo keeps its own HEAD.
                continue;
            }
            // Read the worktree file
            match worktree_bytes_for_index_mode(&file_path, idx_entry.mode) {
                Ok(contents) => {
                    if let Ok(idx_blob) = repo.odb.read(&idx_entry.oid) {
                        if contents != idx_blob.data {
                            return Err(Error::Message(format!("error: Your local changes to the following files would be overwritten by merge:\n\t{path}\nPlease commit your changes or stash them before you merge.")));
                        }
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File doesn't exist in worktree — could be deleted locally
                }
                Err(e) => return Err(e.into()),
            }
        }
    }

    // Read index commit tree
    let idx_obj = repo.odb.read(index_commit_oid)?;
    let idx_commit = parse_commit(&idx_obj.data)?;
    let idx_tree_entries = flatten_tree_full(&repo.odb, &idx_commit.tree, "")?;
    let idx_map: BTreeMap<String, &FlatTreeEntry> = idx_tree_entries
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    // Determine if HEAD has moved since the stash was created
    let current_head = resolve_head(&repo.git_dir)?;
    let current_head_oid = current_head.oid().copied();
    let head_moved = current_head_oid.as_ref() != Some(head_at_stash);

    // Current HEAD tree (for three-way merge when HEAD moved, and for index reset without --index).
    let current_head_flat: Vec<FlatTreeEntry> = if let Some(ref h) = current_head_oid {
        let head_obj = repo.odb.read(h)?;
        let head_commit = parse_commit(&head_obj.data)?;
        flatten_tree_full(&repo.odb, &head_commit.tree, "")?
    } else {
        Vec::new()
    };
    let cur_head_map: BTreeMap<String, &FlatTreeEntry> = current_head_flat
        .iter()
        .map(|e| (e.path.clone(), e))
        .collect();

    // Build current HEAD tree map for three-way merge (OID only)
    let current_tree_map: BTreeMap<String, ObjectId> = if head_moved {
        current_head_flat
            .iter()
            .map(|e| (e.path.clone(), e.oid))
            .collect()
    } else {
        BTreeMap::new()
    };

    let mut has_conflicts = false;
    let mut new_index = current_index.clone();

    // Pre-check: detect type conflicts where the stash wants to place a FILE
    // at a path that is currently a DIRECTORY in the worktree, or vice-versa.
    // We must check BEFORE removing anything (deletions below may clear dirs).
    for (path, change) in &wt_changes {
        if let Some(entry) = change {
            if entry.mode == MODE_GITLINK {
                continue;
            }
            let file_path = work_tree.join(path);
            if file_path.is_dir() {
                // A file from the stash conflicts with a directory in the worktree.
                // Mark as conflicted and remove the directory so we can write the file.
                has_conflicts = true;
                let _ = fs::remove_dir_all(&file_path);
            }
        }
    }

    // First pass: process deletions (None entries) before additions to avoid
    // type conflicts (e.g., trying to write a file where a directory exists).
    for (path, change) in &wt_changes {
        if change.is_some() {
            continue;
        }
        let file_path = work_tree.join(path);
        if file_path.is_dir() {
            let git_meta = file_path.join(".git");
            if git_meta.is_file() || git_meta.is_dir() {
                continue;
            }
            let _ = fs::remove_dir_all(&file_path);
        } else {
            let _ = fs::remove_file(&file_path);
        }
        if let Some(parent) = file_path.parent() {
            remove_empty_dirs(parent, work_tree);
        }
    }

    // Apply working tree changes (with three-way merge when HEAD has moved)
    for (path, change) in &wt_changes {
        let file_path = work_tree.join(path);
        match change {
            Some(entry) => {
                if let Some(parent) = file_path.parent() {
                    // If a component of the parent is a file, remove it first
                    let mut cur = work_tree.to_path_buf();
                    if let Ok(rel) = file_path
                        .parent()
                        .unwrap_or(work_tree)
                        .strip_prefix(work_tree)
                    {
                        for comp in rel.components() {
                            cur.push(comp);
                            if cur.exists() && !cur.is_dir() {
                                let _ = fs::remove_file(&cur);
                            }
                        }
                    }
                    fs::create_dir_all(parent)?;
                }
                if entry.mode == MODE_GITLINK {
                    if file_path.is_file() || file_path.is_symlink() {
                        let _ = fs::remove_file(&file_path);
                    } else if file_path.is_dir() {
                        let git_meta = file_path.join(".git");
                        if !(git_meta.is_file() || git_meta.is_dir()) {
                            fs::remove_dir_all(&file_path)?;
                        }
                    }
                    fs::create_dir_all(&file_path)?;
                    continue;
                }

                let stash_blob = repo.odb.read(&entry.oid)?;

                if entry.mode == MODE_SYMLINK {
                    let target = String::from_utf8(stash_blob.data)
                        .map_err(|_| Error::Message("symlink target is not UTF-8".into()))?;
                    if file_path.exists() || file_path.symlink_metadata().is_ok() {
                        let _ = fs::remove_file(&file_path);
                    }
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(&target, &file_path)?;
                    // Windows lacks unprivileged symlinks; keep the worktree
                    // populated by writing the target as a regular file.
                    #[cfg(not(unix))]
                    fs::write(&file_path, target.as_bytes())?;
                } else if head_moved {
                    // Three-way merge: base (head_at_stash), ours (current HEAD), theirs (stash)
                    let base_content = base_map
                        .get(path)
                        .and_then(|e| repo.odb.read(&e.oid).ok())
                        .map(|o| o.data)
                        .unwrap_or_default();
                    let ours_content = current_tree_map
                        .get(path)
                        .and_then(|oid| repo.odb.read(oid).ok())
                        .map(|o| o.data)
                        .unwrap_or_default();
                    let theirs_content = stash_blob.data;

                    // If ours == base, no conflict (only stash changed this file)
                    if ours_content == base_content {
                        write_regular_file_replacing_symlink(&file_path, &theirs_content)?;
                    } else if ours_content == theirs_content {
                        // Both changed the same way, no conflict
                        write_regular_file_replacing_symlink(&file_path, &ours_content)?;
                    } else {
                        // Both sides changed differently — try content merge
                        use crate::merge_file::{merge, ConflictStyle, MergeFavor, MergeInput};
                        let input = MergeInput {
                            base: &base_content,
                            ours: &ours_content,
                            theirs: &theirs_content,
                            label_ours: "Updated upstream",
                            label_base: "Stashed changes",
                            label_theirs: "Stashed changes",
                            favor: MergeFavor::None,
                            style: ConflictStyle::Merge,
                            marker_size: 7,
                            diff_algorithm: None,
                            ignore_all_space: false,
                            ignore_space_change: false,
                            ignore_space_at_eol: false,
                            ignore_cr_at_eol: false,
                        };
                        let output = merge(&input)?;
                        write_regular_file_replacing_symlink(&file_path, &output.content)?;
                        if output.conflicts > 0 {
                            has_conflicts = true;
                            // Write conflict stages to index
                            let path_bytes = path.as_bytes();
                            // Remove existing stage-0 entry
                            new_index
                                .entries
                                .retain(|e| e.path != path_bytes || e.stage() != 0);
                            // Adding non-zero stages drops this path from any valid stage-0
                            // cache-tree; invalidate it (Git's add_index_entry ->
                            // cache_tree_invalidate_path) so a stale TREE extension is not written
                            // alongside the conflicted index (otherwise GIT_TEST_CHECK_CACHE_TREE
                            // rejects it with "corrupted cache-tree has entries not present in
                            // index"; t7600 'merge with conflicted --autostash changes').
                            new_index.invalidate_cache_tree_for_path(path_bytes);
                            // Add stage entries
                            if let Some(base_entry) = base_map.get(path) {
                                add_stage_entry(
                                    &mut new_index,
                                    path_bytes,
                                    &base_entry.oid,
                                    base_entry.mode,
                                    1,
                                );
                            }
                            if let Some(ours_oid) = current_tree_map.get(path) {
                                let mode = current_index
                                    .get(path_bytes, 0)
                                    .map(|e| e.mode)
                                    .unwrap_or(0o100644);
                                add_stage_entry(&mut new_index, path_bytes, ours_oid, mode, 2);
                            }
                            add_stage_entry(&mut new_index, path_bytes, &entry.oid, entry.mode, 3);
                        }
                    }
                } else {
                    write_regular_file_replacing_symlink(&file_path, &stash_blob.data)?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        if entry.mode == MODE_EXECUTABLE {
                            let perms = std::fs::Permissions::from_mode(0o755);
                            fs::set_permissions(&file_path, perms)?;
                        }
                    }
                }
            }
            None => {
                // Deleted in stash
                let _ = fs::remove_file(&file_path);
                if let Some(parent) = file_path.parent() {
                    remove_empty_dirs(parent, work_tree);
                }
            }
        }
    }

    // Update the index

    if restore_index {
        // --index: restore the index to the stash's index state for changed files
        for (path, idx_entry) in &idx_map {
            let base_oid = base_map.get(path).map(|e| &e.oid);
            if base_oid != Some(&idx_entry.oid) {
                // This file was staged differently from base in the stash
                let path_bytes = path.as_bytes();
                if let Some(ie) = new_index.get_mut(path_bytes, 0) {
                    ie.oid = idx_entry.oid;
                    ie.mode = idx_entry.mode;
                } else {
                    let flags = if path.len() > 0xFFF {
                        0xFFF
                    } else {
                        path.len() as u16
                    };
                    new_index.push_entry_unsorted(IndexEntry {
                        ctime_sec: 0,
                        ctime_nsec: 0,
                        mtime_sec: 0,
                        mtime_nsec: 0,
                        dev: 0,
                        ino: 0,
                        mode: idx_entry.mode,
                        uid: 0,
                        gid: 0,
                        size: 0,
                        oid: idx_entry.oid,
                        flags,
                        flags_extended: None,
                        path: path_bytes.to_vec(),
                        base_index_pos: 0,
                    });
                }
            }
        }
        // Handle files added in the index but not in base
        // (already covered above)
        for path in wt_changes.keys() {
            if let Some(ie) = new_index.get_mut(path.as_bytes(), 0) {
                ie.set_skip_worktree(false);
            }
        }
        new_index.sort();
    } else {
        // Without --index: index tracks current HEAD for paths the stash touched
        // (worktree gets the stashed changes; index matches HEAD at those paths).
        //
        // Exception: paths that exist in the stash index parent but not on **current** HEAD
        // (e.g. a newly `git add`ed file) must be re-staged from the stash index parent
        // (t3903 `stash an added file`).
        let mut touched: BTreeSet<String> = BTreeSet::new();
        for p in wt_changes.keys() {
            touched.insert(p.clone());
        }
        for path in idx_map.keys() {
            if !base_map.contains_key(path) {
                touched.insert(path.clone());
            }
        }
        for path in &touched {
            if let Some(te) = cur_head_map.get(path.as_str()) {
                let path_bytes = path.as_bytes();
                let size = if te.mode == MODE_SYMLINK || te.mode == MODE_GITLINK {
                    0u32
                } else {
                    repo.odb.read(&te.oid)?.data.len() as u32
                };
                let new_entry = IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: te.mode,
                    uid: 0,
                    gid: 0,
                    size,
                    oid: te.oid,
                    flags: path_bytes.len().min(0xFFF) as u16,
                    flags_extended: None,
                    path: path_bytes.to_vec(),
                    base_index_pos: 0,
                };
                // Do not replace unmerged index entries: `stage_file` strips stages 1–3, which
                // would hide merge conflicts after stash apply (t9903 conflict prompt).
                let has_unmerged = new_index
                    .entries
                    .iter()
                    .any(|e| e.path == path_bytes && e.stage() > 0);
                if !has_unmerged {
                    new_index.stage_file(new_entry);
                }
            } else {
                let path_bytes = path.as_bytes();
                let has_unmerged = new_index
                    .entries
                    .iter()
                    .any(|e| e.path == path_bytes && e.stage() > 0);
                if has_unmerged {
                    continue;
                }
                if let Some(ie) = idx_map.get(path.as_str()) {
                    let had_staged = match base_map.get(path.as_str()) {
                        Some(b) => b.oid != ie.oid || b.mode != ie.mode,
                        None => true,
                    };
                    if had_staged {
                        let size = if ie.mode == MODE_SYMLINK || ie.mode == MODE_GITLINK {
                            0u32
                        } else {
                            repo.odb.read(&ie.oid)?.data.len() as u32
                        };
                        new_index.stage_file(IndexEntry {
                            ctime_sec: 0,
                            ctime_nsec: 0,
                            mtime_sec: 0,
                            mtime_nsec: 0,
                            dev: 0,
                            ino: 0,
                            mode: ie.mode,
                            uid: 0,
                            gid: 0,
                            size,
                            oid: ie.oid,
                            flags: path_bytes.len().min(0xFFF) as u16,
                            flags_extended: None,
                            path: path_bytes.to_vec(),
                            base_index_pos: 0,
                        });
                    } else {
                        new_index.remove(path_bytes);
                    }
                } else {
                    new_index.remove(path_bytes);
                }
            }
        }
        new_index.sort();
    }

    if has_conflicts {
        new_index.sort();
        // A conflicted index (unmerged stages) cannot have a valid stage-0 cache-tree; drop the
        // TREE extension so write_index does not persist a stale one (which would fail
        // GIT_TEST_CHECK_CACHE_TREE verification with "corrupted cache-tree has entries not
        // present in index"). Mirrors Git, which only keeps a cache-tree for a fully merged index.
        new_index.clear_cache_tree();
    }
    // Refresh cached stat for entries restored from the stash trees whose worktree content matches
    // the recorded OID, so a following `git diff-files` reflects only genuine differences (t3903
    // 'stash apply --index refreshes the index').
    if !has_conflicts {
        let rules = crate::worktree_rules::WorktreeRules::from_repository(repo, &new_index).ok();
        let rules_arc = rules.map(|r| std::sync::Arc::new(std::sync::Mutex::new(r)));
        let _ = crate::diff::refresh_index_stat_content_verified_with_rules(
            &repo.odb,
            &repo.git_dir,
            &mut new_index,
            work_tree,
            None,
            repo.config().ok().as_deref(),
            None,
            rules_arc.as_ref(),
        )?;
    }
    repo.write_index(&mut new_index)
        .map_err(|e| Error::Message(format!("writing index after stash apply: {e}")))?;

    // Apply untracked files if present (3rd parent)
    if stash_commit.parents.len() >= 3 {
        let ut_oid = &stash_commit.parents[2];
        let ut_obj = repo.odb.read(ut_oid)?;
        let ut_commit = parse_commit(&ut_obj.data)?;
        let ut_entries = flatten_tree_full(&repo.odb, &ut_commit.tree, "")?;
        for entry in &ut_entries {
            let file_path = work_tree.join(&entry.path);
            if let Some(parent) = file_path.parent() {
                fs::create_dir_all(parent)?;
            }
            let blob = repo.odb.read(&entry.oid)?;
            fs::write(&file_path, &blob.data)?;
        }
    }

    Ok(has_conflicts)
}
