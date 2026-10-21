//! Remove and rename paths in the working tree and index (`git rm` / `git mv` semantics).

use std::collections::{BTreeSet, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex};

use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, DiffIndexToWorktreeOptions, DiffStatus,
};
use crate::error::{Error, Result};
use crate::index::{index_file_mtime, Index, IndexEntry, MODE_TREE};
use crate::objects::{parse_commit, ObjectId};
use crate::path_icase::paths_equal;
use crate::path_validate::{validate_path, EntryMode, ProtectOptions};
use crate::pathspec::pathspec_is_exclude;
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
/// is written atomically after the index lock is acquired ([`IndexHandle::begin_update`](crate::index::IndexHandle::begin_update)).
///
/// # Errors
///
/// Returns [`Error::PathspecNoMatch`], [`Error::PathsHaveLocalModifications`],
/// [`Error::NotRemovingRecursively`], or I/O / object-store failures.
pub fn remove_paths(repo: &Repository, opts: &RemoveOptions) -> Result<RemoveOutcome> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    if opts.pathspecs.is_empty() {
        return Err(Error::Message("you must specify path(s) to remove".into()));
    }

    let mut lock = repo.begin_index_update()?;
    let mut index = repo.load_index()?;
    let head_tree = resolve_head_tree(repo)?;
    let tracked_paths = tracked_index_paths(&index);

    let to_remove = collect_removal_paths(repo, &tracked_paths, work_tree, opts)?;

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

    if to_remove.is_empty() {
        return Ok(RemoveOutcome::default());
    }

    let path_bytes: Vec<&[u8]> = to_remove.iter().map(|p| p.as_bytes()).collect();
    index.remove_paths(path_bytes);
    repo.commit_index_update(&mut lock, &mut index, !opts.cached)?;

    if !opts.cached {
        let mut removed_worktree: Vec<PathBuf> = Vec::new();
        for path in &to_remove {
            let abs = work_tree.join(path);
            if abs.is_file() || abs.is_symlink() {
                fs::remove_file(&abs)
                    .map_err(|e| Error::PathError(format!("removing '{}': {e}", path)))?;
                removed_worktree.push(abs);
            }
        }
        for abs in removed_worktree {
            remove_empty_parent_dirs(work_tree, &abs);
        }
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
/// Returns [`Error::InvalidPath`] when `src` or `dst` is absolute or contains `..`,
/// [`Error::Message`] for untracked sources or invalid destinations, or I/O failures.
pub fn move_path(repo: &Repository, src: &str, dst: &str, force: bool) -> Result<MoveOutcome> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("this operation must be run in a work tree".into()))?;

    let mut lock = repo.begin_index_update()?;
    let mut index = repo.load_index()?;
    let src = normalize_and_validate_repo_relative(repo, work_tree, src)?;
    let dst = normalize_and_validate_repo_relative(repo, work_tree, dst)?;

    let src_entries = index_paths_under(&index, &src, true);
    if src_entries.is_empty() {
        return Err(Error::Message(format!(
            "not under version control, source={src}"
        )));
    }

    let (final_dst, dst_is_dir_target) = resolve_move_destination(work_tree, &index, &src, &dst)?;
    let final_dst = normalize_and_validate_repo_relative(repo, work_tree, &final_dst)?;

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

    let ignorecase = repo
        .config()
        .ok()
        .and_then(|cfg| cfg.get_bool("core.ignorecase").and_then(|r| r.ok()))
        .unwrap_or(false);

    if !force
        && destination_exists(
            work_tree,
            &index,
            &final_dst,
            dst_is_dir_target,
            ignorecase,
            &renames,
        )
    {
        return Err(Error::Message(format!(
            "destination exists, source={src}, destination={final_dst}"
        )));
    }

    let force_clear_worktree = if force {
        worktree_paths_to_clear(work_tree, &index, &final_dst)
    } else {
        Vec::new()
    };

    let old_paths: Vec<Vec<u8>> = renames
        .iter()
        .map(|(from, _)| from.as_bytes().to_vec())
        .collect();
    let mut new_entries = Vec::with_capacity(renames.len());
    for (from, to) in &renames {
        let old = index.get(from.as_bytes(), 0).ok_or_else(|| {
            Error::Message(format!("internal error: missing index entry for '{from}'"))
        })?;
        new_entries.push(entry_with_relpath(old.clone(), to));
    }

    let cleared_backup = if force && !force_clear_worktree.is_empty() {
        backup_and_remove_worktree_paths(work_tree, &force_clear_worktree)?
    } else {
        Vec::new()
    };

    if let Err(err) = rename_worktree_paths(work_tree, &src, &final_dst, &renames, ignorecase) {
        restore_worktree_backups(work_tree, &cleared_backup)?;
        return Err(err);
    }

    if force {
        clear_destination_index(&mut index, &final_dst);
    }

    let old_refs: Vec<&[u8]> = old_paths.iter().map(|p| p.as_slice()).collect();
    index.remove_paths_and_insert(old_refs, new_entries);

    repo.flush_index_update(&mut lock, &mut index)?;
    repo.finish_index_update(&mut lock, true)?;

    Ok(MoveOutcome {
        from: src,
        to: final_dst,
    })
}

fn resolve_head_tree(repo: &Repository) -> Result<Option<ObjectId>> {
    match resolve_head(repo.git_dir())? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => {
            let obj = repo.odb().read(&oid)?;
            let commit = parse_commit(&obj.data)?;
            Ok(Some(commit.tree))
        }
        HeadState::Branch { .. } => Ok(None),
        HeadState::Invalid => Err(Error::Message("HEAD is in an unknown state".into())),
    }
}

/// Stage-0 paths plus any path that appears in the index at any merge stage.
fn tracked_index_paths(index: &Index) -> BTreeSet<String> {
    index
        .entries
        .iter()
        .map(|e| String::from_utf8_lossy(&e.path).into_owned())
        .collect()
}

fn collect_removal_paths(
    repo: &Repository,
    tracked: &BTreeSet<String>,
    _work_tree: &Path,
    opts: &RemoveOptions,
) -> Result<Vec<String>> {
    let pathspec_ctx = repo.pathspec_match_context();
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
            .filter(|path| {
                crate::pathspec::matches_pathspec_list_with_context(
                    path,
                    std::slice::from_ref(resolved),
                    pathspec_ctx,
                )
            })
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

        if !opts.recursive && has_child_paths(spec, tracked) {
            let prefix = format!("{spec}/");
            if matches.iter().any(|path| path.starts_with(&prefix)) {
                return Err(Error::NotRemovingRecursively(resolved.clone()));
            }
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
    let staged = diff_index_to_tree(repo.odb(), index, head_tree, false)?;
    let index_mtime = index_file_mtime(&repo.index_path());
    let worktree_rules = crate::worktree_rules::WorktreeRules::from_repository(repo, index)?;
    let rules_arc = Arc::new(Mutex::new(worktree_rules));
    let diff_opts = DiffIndexToWorktreeOptions {
        index_mtime,
        repository_git_dir: Some(repo.git_dir().to_path_buf()),
        refresh_index_stat_in_pass: true,
        config: repo.config().ok(),
        worktree_rules: Some(Arc::clone(&rules_arc)),
        command_runner: Some(repo.command_runner()),
        ..Default::default()
    };
    let (unstaged, _) =
        diff_index_to_worktree_with_options(repo.odb(), index, work_tree, diff_opts)?;

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

#[derive(Debug, Clone)]
enum WorktreePathBackup {
    File {
        rel: String,
        bytes: Vec<u8>,
        #[cfg(unix)]
        mode: u32,
    },
    Symlink {
        rel: String,
        target: PathBuf,
    },
}

impl WorktreePathBackup {
    fn rel(&self) -> &str {
        match self {
            Self::File { rel, .. } | Self::Symlink { rel, .. } => rel,
        }
    }
}

fn snapshot_worktree_path(work_tree: &Path, path: &str) -> Result<Option<WorktreePathBackup>> {
    let abs = work_tree.join(path);
    if abs.is_symlink() {
        let target = fs::read_link(&abs)
            .map_err(|e| Error::PathError(format!("reading symlink '{path}': {e}")))?;
        return Ok(Some(WorktreePathBackup::Symlink {
            rel: path.to_string(),
            target,
        }));
    }
    if abs.is_file() {
        let bytes =
            fs::read(&abs).map_err(|e| Error::PathError(format!("reading '{path}': {e}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&abs)
                .map_err(|e| Error::PathError(format!("reading metadata for '{path}': {e}")))?
                .permissions()
                .mode()
                & 0o7777;
            return Ok(Some(WorktreePathBackup::File {
                rel: path.to_string(),
                bytes,
                mode,
            }));
        }
        #[cfg(not(unix))]
        {
            return Ok(Some(WorktreePathBackup::File {
                rel: path.to_string(),
                bytes,
            }));
        }
    }
    Ok(None)
}

fn remove_backed_up_worktree_path(work_tree: &Path, backup: &WorktreePathBackup) -> Result<()> {
    let path = backup.rel();
    let abs = work_tree.join(path);
    fs::remove_file(&abs).map_err(|e| Error::PathError(format!("removing '{path}': {e}")))?;
    remove_empty_parent_dirs(work_tree, &abs);
    Ok(())
}

fn backup_and_remove_worktree_paths(
    work_tree: &Path,
    paths: &[String],
) -> Result<Vec<WorktreePathBackup>> {
    let mut snapshots = Vec::with_capacity(paths.len());
    for path in paths {
        if let Some(backup) = snapshot_worktree_path(work_tree, path)? {
            snapshots.push(backup);
        }
    }

    let mut removed: Vec<WorktreePathBackup> = Vec::new();
    for backup in &snapshots {
        match remove_backed_up_worktree_path(work_tree, backup) {
            Ok(()) => removed.push(backup.clone()),
            Err(err) => {
                let _ = restore_worktree_backups(work_tree, &removed);
                return Err(err);
            }
        }
    }
    Ok(snapshots)
}

fn restore_worktree_backups(work_tree: &Path, backups: &[WorktreePathBackup]) -> Result<()> {
    for backup in backups {
        match backup {
            WorktreePathBackup::File {
                rel,
                bytes,
                #[cfg(unix)]
                mode,
            } => {
                let abs = work_tree.join(rel);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        Error::PathError(format!("creating parent for '{rel}': {e}"))
                    })?;
                }
                fs::write(&abs, bytes)
                    .map_err(|e| Error::PathError(format!("restoring '{rel}': {e}")))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let mut perms = fs::metadata(&abs)
                        .map_err(|e| {
                            Error::PathError(format!("reading metadata for '{rel}': {e}"))
                        })?
                        .permissions();
                    perms.set_mode(*mode);
                    fs::set_permissions(&abs, perms).map_err(|e| {
                        Error::PathError(format!("restoring mode for '{rel}': {e}"))
                    })?;
                }
            }
            #[cfg(not(unix))]
            WorktreePathBackup::File { rel, bytes } => {
                let abs = work_tree.join(rel);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        Error::PathError(format!("creating parent for '{rel}': {e}"))
                    })?;
                }
                fs::write(&abs, bytes)
                    .map_err(|e| Error::PathError(format!("restoring '{rel}': {e}")))?;
            }
            WorktreePathBackup::Symlink { rel, target } => {
                let abs = work_tree.join(rel);
                if let Some(parent) = abs.parent() {
                    fs::create_dir_all(parent).map_err(|e| {
                        Error::PathError(format!("creating parent for '{rel}': {e}"))
                    })?;
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::symlink;
                    symlink(target, &abs)
                        .map_err(|e| Error::PathError(format!("restoring symlink '{rel}': {e}")))?;
                }
                #[cfg(not(unix))]
                {
                    return Err(Error::Message(format!(
                        "cannot restore symlink '{rel}' on this platform"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Normalize a repository-relative path for rm/mv, rejecting escapes and absolutes.
///
/// Absolute paths (including paths inside the work tree) and any `..` segment
/// are rejected before normalization so callers cannot bypass prefix checks.
///
/// # Errors
///
/// Returns [`Error::InvalidPath`] when `path` is empty, absolute, or contains `..`.
fn normalize_and_validate_repo_relative(
    repo: &Repository,
    _work_tree: &Path,
    path: &str,
) -> Result<String> {
    if path.is_empty() {
        return Err(Error::InvalidPath("empty path".into()));
    }
    if Path::new(path).is_absolute() {
        return Err(Error::InvalidPath(format!(
            "'{path}' is outside repository"
        )));
    }
    let with_slashes = path.replace('\\', "/");
    if with_slashes.starts_with('/') {
        return Err(Error::InvalidPath(format!(
            "'{path}' is outside repository"
        )));
    }
    for component in Path::new(&with_slashes).components() {
        if matches!(component, Component::ParentDir) {
            return Err(Error::InvalidPath(format!(
                "'{path}' is outside repository"
            )));
        }
    }
    let rel = with_slashes.trim_start_matches("./");
    if rel.is_empty() {
        return Err(Error::InvalidPath("empty path".into()));
    }
    let opts = ProtectOptions::from_config(&*repo.config()?);
    validate_path(rel.as_bytes(), EntryMode::new(0o100644), opts)
        .map_err(|reason| Error::invalid_path_component(rel, reason))?;
    Ok(rel.to_string())
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

fn destination_exists(
    work_tree: &Path,
    index: &Index,
    dst: &str,
    _dst_was_dir: bool,
    ignorecase: bool,
    renames: &[(String, String)],
) -> bool {
    if ignorecase && case_only_rename_to(renames, dst) {
        return index_has_unrelated_entries(index, dst, ignorecase, renames);
    }

    let abs = work_tree.join(dst);
    if abs.exists() {
        if ignorecase && rename_sources_cover_path(renames, dst) {
            return index_has_unrelated_entries(index, dst, ignorecase, renames);
        }
        return true;
    }

    index_has_unrelated_entries(index, dst, ignorecase, renames)
}

fn case_only_rename_to(renames: &[(String, String)], dst: &str) -> bool {
    renames.iter().any(|(from, to)| {
        to == dst && from != to && paths_equal(from.as_bytes(), to.as_bytes(), true)
    })
}

fn rename_sources_cover_path(renames: &[(String, String)], path: &str) -> bool {
    renames
        .iter()
        .any(|(from, _)| paths_equal(from.as_bytes(), path.as_bytes(), true))
}

fn index_has_unrelated_entries(
    index: &Index,
    dst: &str,
    ignorecase: bool,
    renames: &[(String, String)],
) -> bool {
    let rename_from: HashSet<&str> = renames.iter().map(|(from, _)| from.as_str()).collect();

    if ignorecase {
        return index.entries.iter().filter(|e| e.stage() == 0).any(|e| {
            let path = String::from_utf8_lossy(&e.path);
            if !index_path_at_destination(&path, dst) {
                return false;
            }
            !rename_from.contains(path.as_ref())
        });
    }

    let mut paths = index_paths_under(index, dst, true);
    if paths.is_empty() && index.get(dst.as_bytes(), 0).is_some() {
        paths.push(dst.to_string());
    }
    paths.iter().any(|p| !rename_from.contains(p.as_str()))
}

fn index_path_at_destination(path: &str, dst: &str) -> bool {
    if paths_equal(path.as_bytes(), dst.as_bytes(), true) {
        return true;
    }
    if path == dst {
        return true;
    }
    let plen = dst.len();
    path.len() > plen && path.starts_with(dst) && path.as_bytes()[plen] == b'/'
}

fn worktree_paths_to_clear(work_tree: &Path, index: &Index, dst: &str) -> Vec<String> {
    let mut paths = index_paths_under(index, dst, true);
    if paths.is_empty() {
        let abs = work_tree.join(dst);
        if abs.is_file() || abs.is_symlink() {
            paths.push(dst.to_string());
        }
    }
    paths
}

fn clear_destination_index(index: &mut Index, dst: &str) {
    let paths = index_paths_under(index, dst, true);
    if paths.is_empty() {
        index.remove(dst.as_bytes());
    } else {
        index.remove_paths(paths.iter().map(|p| p.as_bytes()));
    }
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

fn rename_worktree_entry(from: &Path, to: &Path, ignorecase: bool) -> std::io::Result<()> {
    if from == to {
        return Ok(());
    }
    let from_name = from.to_string_lossy();
    let to_name = to.to_string_lossy();
    if ignorecase && from_name.eq_ignore_ascii_case(to_name.as_ref()) && from != to {
        case_only_worktree_rename(from, to)
    } else {
        fs::rename(from, to)
    }
}

fn case_only_worktree_rename(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};

    static CASE_MV_TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let parent = to.parent().unwrap_or_else(|| Path::new("."));
    let salt = CASE_MV_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    for attempt in 0..64 {
        let temp_path = parent.join(format!(
            ".grit-mv-{}-{}-{}",
            std::process::id(),
            salt,
            attempt
        ));
        if temp_path.exists() {
            continue;
        }
        match fs::rename(from, &temp_path) {
            Ok(()) => match fs::rename(&temp_path, to) {
                Ok(()) => return Ok(()),
                Err(e) => {
                    if fs::rename(&temp_path, from).is_err() {
                        return Err(std::io::Error::new(
                            e.kind(),
                            format!(
                                "renaming back to '{}' after failed final case-only step: \
                                 source may be at '{}'",
                                from.display(),
                                temp_path.display()
                            ),
                        ));
                    }
                    return Err(e);
                }
            },
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(std::io::Error::other(
        "could not allocate a unique temporary path for case-only rename",
    ))
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
    ignorecase: bool,
) -> Result<()> {
    if renames.len() == 1 && renames[0].0 == src {
        let from = work_tree.join(&renames[0].0);
        let to = work_tree.join(&renames[0].1);
        if let Some(parent) = to.parent() {
            fs::create_dir_all(parent).map_err(|e| {
                Error::PathError(format!("creating parent for '{}': {e}", renames[0].1))
            })?;
        }
        rename_worktree_entry(&from, &to, ignorecase).map_err(|e| {
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
        rename_worktree_entry(&src_abs, &dst_abs, ignorecase).map_err(|e| {
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
            rename_worktree_entry(&from_abs, &to_abs, ignorecase)
                .map_err(|e| Error::PathError(format!("renaming '{from}' -> '{to}': {e}")))?;
            remove_empty_parent_dirs(work_tree, &from_abs);
        }
    }
    Ok(())
}

fn entry_with_relpath(mut entry: IndexEntry, path: &str) -> IndexEntry {
    entry.path = path.as_bytes().to_vec();
    let path_len = entry.path.len().min(0xFFF) as u16;
    entry.flags = (entry.flags & !0x0FFF) | path_len;
    entry
}

#[cfg(test)]
mod path_validation_tests {
    use super::normalize_and_validate_repo_relative;
    use crate::error::Error;
    use crate::repo::{InitOptions, Repository};

    fn test_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    #[test]
    fn rejects_absolute_paths() {
        let (_dir, repo) = test_repo();
        let wt = repo.work_tree().expect("worktree");
        let err = normalize_and_validate_repo_relative(&repo, wt, "/tmp/repo/destination")
            .expect_err("abs");
        assert!(matches!(err, Error::InvalidPath(_)));
    }

    #[test]
    fn rejects_internal_parent_components() {
        let (_dir, repo) = test_repo();
        let wt = repo.work_tree().expect("worktree");
        let err = normalize_and_validate_repo_relative(&repo, wt, "dir/../source").expect_err("..");
        assert!(matches!(err, Error::InvalidPath(_)));
    }

    #[test]
    fn accepts_simple_relative_paths() {
        let (_dir, repo) = test_repo();
        let wt = repo.work_tree().expect("worktree");
        assert_eq!(
            normalize_and_validate_repo_relative(&repo, wt, "./nested/file.txt").expect("ok"),
            "nested/file.txt"
        );
    }
}

#[cfg(test)]
mod destination_exists_tests {
    use super::{
        case_only_rename_to, destination_exists, index_has_unrelated_entries,
        rename_sources_cover_path,
    };
    use crate::index::Index;
    use crate::index::IndexEntry;
    use crate::objects::ObjectId;

    fn dummy_entry(path: &str) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: 0o100644,
            uid: 0,
            gid: 0,
            size: 0,
            oid: ObjectId::from_bytes(&[0u8; 20]).unwrap(),
            flags: 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn case_only_rename_helpers() {
        let renames = vec![
            ("README.md".into(), "readme.md".into()),
            ("src/A.rs".into(), "src/a.rs".into()),
        ];
        assert!(case_only_rename_to(&renames, "readme.md"));
        assert!(!case_only_rename_to(&renames, "other.md"));
        assert!(rename_sources_cover_path(&renames, "README.md"));
        assert!(!rename_sources_cover_path(&renames, "missing.md"));
    }

    #[test]
    fn ignorecase_index_collision_allows_case_only_target() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let wt = tmp.path();
        std::fs::write(wt.join("README.md"), b"x\n").unwrap();
        let mut index = Index::new();
        index.push_entry_unsorted(dummy_entry("README.md"));
        let renames = vec![("README.md".into(), "readme.md".into())];
        assert!(
            !destination_exists(wt, &index, "readme.md", false, true, &renames),
            "case-only rename must not treat the source path as a blocking destination"
        );
    }

    #[test]
    fn ignorecase_index_blocks_unrelated_destination() {
        let mut index = Index::new();
        index.push_entry_unsorted(dummy_entry("README.md"));
        index.push_entry_unsorted(dummy_entry("readme.md"));
        let renames = vec![("README.md".into(), "readme.md".into())];
        assert!(
            index_has_unrelated_entries(&index, "readme.md", true, &renames),
            "distinct index spellings at the same icase path must block"
        );
    }
}

#[cfg(test)]
mod index_lock_transaction_tests {
    use std::fs;

    use crate::porcelain::add::{stage, StageOptions};
    use crate::progress::NullProgress;
    use crate::repo::{init_repository, set_test_inject_index_write_fail, Repository};

    use super::{move_path, remove_paths, RemoveOptions};

    fn init_repo(root: &std::path::Path) -> Repository {
        init_repository(root, false, "main", None, Default::default()).expect("init")
    }

    #[test]
    fn remove_paths_injected_index_write_failure_leaves_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        set_test_inject_index_write_fail(true);
        let err = remove_paths(
            &repo,
            &RemoveOptions {
                pathspecs: vec!["a.txt".into()],
                pathspec_sources: vec!["a.txt".into()],
                ..Default::default()
            },
        )
        .expect_err("injected write failure");
        set_test_inject_index_write_fail(false);
        assert!(matches!(err, crate::error::Error::Io(_)));
        assert!(root.join("a.txt").is_file());
    }

    #[test]
    fn move_path_injected_index_write_failure_leaves_worktree() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let root = tmp.path();
        let repo = init_repo(root);
        fs::write(root.join("a.txt"), b"1\n").unwrap();
        stage(&repo, &StageOptions::default(), &mut NullProgress).unwrap();

        set_test_inject_index_write_fail(true);
        let err = move_path(&repo, "a.txt", "b.txt", false).expect_err("injected write failure");
        set_test_inject_index_write_fail(false);
        assert!(matches!(err, crate::error::Error::Io(_)));
        assert!(root.join("a.txt").is_file());
        assert!(!root.join("b.txt").exists());
    }
}
