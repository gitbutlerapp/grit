//! Remove and rename paths in the working tree and index (`git rm` / `git mv` semantics).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, Index, IndexEntry, MODE_TREE};
use crate::objects::{parse_commit, ObjectId};
use crate::pathspec::{matches_pathspec_list, pathspec_is_exclude};
use crate::porcelain::checkout::remove_empty_parent_dirs;
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

/// Options for [`remove_paths`].
#[derive(Debug, Clone, Default)]
pub struct RemoveOptions {
    /// Resolved pathspec patterns (repository-relative).
    pub pathspecs: Vec<String>,
    /// Original pathspec argv fragments for errors (defaults to `pathspecs` when empty).
    pub pathspec_sources: Vec<String>,
    /// Remove from the index only; leave the working tree file in place.
    pub cached: bool,
    /// Remove even when the path has staged or unstaged local modifications.
    pub force: bool,
    /// Allow removing directories (all tracked paths under a directory prefix).
    pub recursive: bool,
}

/// Result of [`remove_paths`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemoveOutcome {
    /// Repository-relative paths removed from the index (and usually the work tree).
    pub removed: Vec<String>,
}

/// Result of [`move_path`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoveOutcome {
    /// Source path before the move.
    pub from: String,
    /// Destination path after the move.
    pub to: String,
}

/// Remove paths matching `opts.pathspecs` from the index and (unless `cached`) the work tree.
///
/// Refuses when a tracked path has staged or unstaged modifications unless `force` is set.
/// Directory prefixes require `recursive`. Empty parent directories are removed after
/// deleting files. Cache-tree entries for touched paths are invalidated before the index
/// is written atomically through [`Repository::write_index`].
///
/// # Errors
///
/// Returns [`Error::PathspecNoMatch`], [`Error::PathsHaveLocalModifications`],
/// [`Error::NotRemovingRecursively`], or I/O / object-store failures.
pub fn remove_paths(repo: &Repository, opts: &RemoveOptions) -> Result<RemoveOutcome> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    if opts.pathspecs.is_empty() {
        return Err(Error::Message("you must specify path(s) to remove".into()));
    }

    let mut index = repo.load_index()?;
    let head_tree = resolve_head_tree(repo)?;
    let tracked_paths = tracked_stage0_paths(&index);

    let to_remove = collect_removal_paths(&tracked_paths, work_tree, opts)?;

    if !opts.force {
        let modified =
            paths_with_local_modifications(repo, &mut index, head_tree.as_ref(), work_tree)?;
        let blocked: Vec<String> = to_remove
            .iter()
            .filter(|p| path_or_ancestor_modified(p, &modified))
            .cloned()
            .collect();
        if !blocked.is_empty() {
            return Err(Error::PathsHaveLocalModifications { paths: blocked });
        }
    }

    let mut removed_worktree: Vec<PathBuf> = Vec::new();
    if !opts.cached {
        for path in &to_remove {
            let abs = work_tree.join(path);
            if abs.is_file() || abs.is_symlink() {
                fs::remove_file(&abs)
                    .map_err(|e| Error::PathError(format!("removing '{}': {e}", path)))?;
                removed_worktree.push(abs);
            } else if abs.is_dir() {
                // Tracked directory entries are removed via their file paths below.
                continue;
            }
        }
    }

    let path_bytes: Vec<&[u8]> = to_remove.iter().map(|p| p.as_bytes()).collect();
    index.remove_paths(path_bytes);

    if !opts.cached {
        for abs in &removed_worktree {
            remove_empty_parent_dirs(work_tree, abs);
        }
    }

    if !to_remove.is_empty() {
        repo.write_index(&mut index)?;
    }

    Ok(RemoveOutcome { removed: to_remove })
}

/// Rename `src` to `dst` in the working tree and index, preserving stat data and object ids.
///
/// Supports moving into an existing directory and moving whole directory trees. Refuses
/// untracked sources, existing destinations without `force`, and moves into a subdirectory
/// of the source directory.
///
/// # Errors
///
/// Returns [`Error::Message`] for untracked sources or invalid destinations, or I/O failures.
pub fn move_path(repo: &Repository, src: &str, dst: &str, force: bool) -> Result<MoveOutcome> {
    let work_tree = repo
        .work_tree
        .as_deref()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    let mut index = repo.load_index()?;
    let src = normalize_repo_path(src);
    let dst = normalize_repo_path(dst);

    let src_entries = index_paths_under(&index, &src, true);
    if src_entries.is_empty() {
        return Err(Error::Message(format!(
            "not under version control, source={src}"
        )));
    }

    let (final_dst, dst_is_dir_target) = resolve_move_destination(work_tree, &index, &src, &dst)?;

    if is_subdirectory(&final_dst, &src) {
        return Err(Error::Message(format!(
            "cannot move '{src}' to a subdirectory of itself, '{final_dst}'"
        )));
    }

    if final_dst == src {
        return Err(Error::Message(format!(
            "cannot move into itself, source={src}, destination={final_dst}"
        )));
    }

    let renames = plan_index_renames(&index, &src, &final_dst)?;
    if renames.is_empty() {
        return Err(Error::Message(format!("nothing to move, source={src}")));
    }

    ensure_worktree_move_sources_exist(work_tree, &renames)?;

    if !force && destination_exists(work_tree, &index, &final_dst, dst_is_dir_target) {
        return Err(Error::Message(format!(
            "destination exists, source={src}, destination={final_dst}"
        )));
    }

    if force {
        clear_destination(work_tree, &mut index, &final_dst)?;
    }

    rename_worktree_paths(work_tree, &src, &final_dst, &renames)?;

    let old_paths: Vec<Vec<u8>> = renames
        .iter()
        .map(|(from, _)| from.as_bytes().to_vec())
        .collect();
    let mut new_entries = Vec::with_capacity(renames.len());
    for (from, to) in renames {
        let old = index.get(from.as_bytes(), 0).ok_or_else(|| {
            Error::Message(format!("internal error: missing index entry for '{from}'"))
        })?;
        new_entries.push(entry_with_relpath(old.clone(), &to));
    }

    let old_refs: Vec<&[u8]> = old_paths.iter().map(|p| p.as_slice()).collect();
    index.remove_paths_and_insert(old_refs, new_entries);

    repo.write_index(&mut index)?;

    Ok(MoveOutcome {
        from: src,
        to: final_dst,
    })
}

fn resolve_head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    match resolve_head(&repo.git_dir)? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => {
            let obj = repo.odb.read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            Ok(Some(commit.tree))
        }
        HeadState::Branch { .. } => Ok(None),
        HeadState::Invalid => Err(Error::Message("HEAD is in an unknown state".into())),
    }
}

fn tracked_stage0_paths(index: &Index) -> BTreeSet<String> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect()
}

fn collect_removal_paths(
    tracked: &BTreeSet<String>,
    _work_tree: &Path,
    opts: &RemoveOptions,
) -> Result<Vec<String>> {
    let sources = if opts.pathspec_sources.is_empty() {
        &opts.pathspecs
    } else {
        &opts.pathspec_sources
    };

    let mut selected: BTreeSet<String> = BTreeSet::new();
    for (label, resolved) in sources.iter().zip(opts.pathspecs.iter()) {
        if pathspec_is_exclude(label) {
            continue;
        }
        let spec = resolved.trim_end_matches('/');
        let matches: Vec<String> = tracked
            .iter()
            .filter(|path| matches_pathspec_list(path, std::slice::from_ref(resolved)))
            .cloned()
            .collect();

        if matches.is_empty() {
            let prefix = format!("{spec}/");
            let under: Vec<String> = tracked
                .iter()
                .filter(|path| path.starts_with(&prefix))
                .cloned()
                .collect();
            if under.is_empty() {
                return Err(Error::PathspecNoMatch {
                    spec: label.clone(),
                });
            }
            if !opts.recursive {
                return Err(Error::NotRemovingRecursively(resolved.clone()));
            }
            selected.extend(under);
            continue;
        }

        for path in matches {
            if has_child_paths(&path, tracked) && !opts.recursive {
                return Err(Error::NotRemovingRecursively(path.clone()));
            }
            selected.insert(path.clone());
            if opts.recursive {
                let prefix = format!("{path}/");
                for child in tracked.iter().filter(|p| p.starts_with(&prefix)) {
                    selected.insert((*child).clone());
                }
            }
        }
    }

    let mut out: Vec<String> = selected.into_iter().collect();
    out.sort();
    Ok(out)
}

fn has_child_paths(prefix: &str, tracked: &BTreeSet<String>) -> bool {
    let plen = prefix.len();
    tracked
        .iter()
        .any(|p| p.len() > plen && p.starts_with(prefix) && p.as_bytes()[plen] == b'/')
}

fn paths_with_local_modifications(
    repo: &Repository,
    index: &mut Index,
    head_tree: Option<&ObjectId>,
    work_tree: &Path,
) -> Result<HashSet<String>> {
    let staged = diff_index_to_tree(&repo.odb, index, head_tree, false)?;
    let index_mtime = index_file_mtime(&repo.index_path());
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        repository_git_dir: Some(repo.git_dir.clone()),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(&repo.odb, index, work_tree, diff_opts)?;

    let mut modified = HashSet::new();
    for entry in staged {
        if entry.status != DiffStatus::Unmerged {
            modified.insert(entry.path().to_string());
        }
    }
    for entry in unstaged {
        if entry.status != DiffStatus::Unmerged {
            modified.insert(entry.path().to_string());
        }
    }
    Ok(modified)
}

fn path_or_ancestor_modified(path: &str, modified: &HashSet<String>) -> bool {
    if modified.contains(path) {
        return true;
    }
    modified
        .iter()
        .any(|m| m.starts_with(path) && m.as_bytes().get(path.len()) == Some(&b'/'))
}

fn normalize_repo_path(path: &str) -> String {
    path.trim_start_matches("./").replace('\\', "/")
}

fn index_paths_under(index: &Index, prefix: &str, include_exact: bool) -> Vec<String> {
    let plen = prefix.len();
    let mut paths: Vec<String> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .filter_map(|e| {
            let p = String::from_utf8_lossy(&e.path);
            if p == prefix {
                return include_exact.then(|| p.into_owned());
            }
            if p.len() > plen && p.starts_with(prefix) && p.as_bytes()[plen] == b'/' {
                return Some(p.into_owned());
            }
            None
        })
        .collect();
    paths.sort();
    paths.dedup();
    paths
}

fn resolve_move_destination(
    work_tree: &Path,
    index: &Index,
    src: &str,
    dst: &str,
) -> Result<(String, bool)> {
    let dst_abs = work_tree.join(dst);
    let dst_is_dir = dst_abs.is_dir()
        || index_paths_under(index, dst, false)
            .iter()
            .any(|p| p.starts_with(dst))
        || index
            .get(dst.as_bytes(), 0)
            .is_some_and(|e| e.mode == MODE_TREE);

    if dst_is_dir {
        let base = Path::new(src)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(src);
        Ok((format!("{dst}/{base}"), true))
    } else {
        Ok((dst.to_string(), false))
    }
}

fn destination_exists(work_tree: &Path, index: &Index, dst: &str, _dst_was_dir: bool) -> bool {
    let abs = work_tree.join(dst);
    if abs.exists() {
        return true;
    }
    index.get(dst.as_bytes(), 0).is_some() || !index_paths_under(index, dst, false).is_empty()
}

fn clear_destination(work_tree: &Path, index: &mut Index, dst: &str) -> Result<()> {
    let paths = index_paths_under(index, dst, true);
    if paths.is_empty() && index.get(dst.as_bytes(), 0).is_none() {
        let abs = work_tree.join(dst);
        if abs.is_file() || abs.is_symlink() {
            fs::remove_file(&abs)
                .map_err(|e| Error::PathError(format!("removing '{dst}': {e}")))?;
            remove_empty_parent_dirs(work_tree, &abs);
        }
        return Ok(());
    }
    for path in &paths {
        let abs = work_tree.join(path);
        if abs.is_file() || abs.is_symlink() {
            let _ = fs::remove_file(&abs);
        }
    }
    index.remove_paths(paths.iter().map(|p| p.as_bytes()));
    Ok(())
}

fn is_subdirectory(candidate: &str, parent_dir: &str) -> bool {
    if candidate == parent_dir {
        return false;
    }
    let plen = parent_dir.len();
    candidate.starts_with(parent_dir) && candidate.as_bytes().get(plen) == Some(&b'/')
}

fn ensure_worktree_move_sources_exist(
    work_tree: &Path,
    renames: &[(String, String)],
) -> Result<()> {
    for (from, to) in renames {
        let abs = work_tree.join(from);
        if abs.is_file() || abs.is_symlink() {
            continue;
        }
        if abs.is_dir() {
            continue;
        }
        return Err(Error::Message(format!(
            "bad source, source={from}, destination={to}"
        )));
    }
    Ok(())
}

fn plan_index_renames(index: &Index, src: &str, final_dst: &str) -> Result<Vec<(String, String)>> {
    let src_paths = index_paths_under(index, src, true);
    let mut renames = Vec::with_capacity(src_paths.len());
    for from in src_paths {
        let suffix = from.strip_prefix(src).unwrap_or("");
        let to = if suffix.is_empty() {
            final_dst.to_string()
        } else if let Some(stripped) = suffix.strip_prefix('/') {
            format!("{final_dst}/{stripped}")
        } else {
            format!("{final_dst}{suffix}")
        };
        renames.push((from, to));
    }
    Ok(renames)
}

fn rename_worktree_paths(
    work_tree: &Path,
    src: &str,
    final_dst: &str,
    renames: &[(String, String)],
) -> Result<()> {
    if renames.len() == 1 && renames[0].0 == src {
        let from = work_tree.join(&renames[0].0);
        let to = work_tree.join(&renames[0].1);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::PathError(format!("creating parent for '{}': {e}", renames[0].1))
            })?;
        }
        fs::rename(&from, &to).map_err(|e| {
            Error::PathError(format!(
                "renaming '{}' -> '{}': {e}",
                renames[0].0, renames[0].1
            ))
        })?;
        remove_empty_parent_dirs(work_tree, &from);
        return Ok(());
    }

    let src_abs = work_tree.join(src);
    let dst_abs = work_tree.join(final_dst);
    if src_abs.is_dir() {
        if let Some(parent) = dst_abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::PathError(format!("creating parent for '{final_dst}': {e}")))?;
        }
        fs::rename(&src_abs, &dst_abs).map_err(|e| {
            Error::PathError(format!("renaming directory '{src}' -> '{final_dst}': {e}"))
        })?;
        remove_empty_parent_dirs(work_tree, &src_abs);
        return Ok(());
    }

    for (from, to) in renames {
        let from_abs = work_tree.join(from);
        let to_abs = work_tree.join(to);
        if let Some(parent) = to_abs.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| Error::PathError(format!("creating parent for '{to}': {e}")))?;
        }
        if from_abs.exists() {
            fs::rename(&from_abs, &to_abs)
                .map_err(|e| Error::PathError(format!("renaming '{from}' -> '{to}': {e}")))?;
            remove_empty_parent_dirs(work_tree, &from_abs);
        }
    }
    Ok(())
}

fn entry_with_relpath(mut entry: IndexEntry, path: &str) -> IndexEntry {
    let stage = entry.stage();
    entry.path = path.as_bytes().to_vec();
    let path_len = entry.path.len().min(0xFFF) as u16;
    entry.flags = path_len | ((stage as u16) << 12);
    entry
}
