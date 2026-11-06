//! Remove untracked (and optionally ignored) paths from the work tree.
//!
//! [`clean_untracked`] reuses the same untracked/ignored walk as [`super::status`]
//! ([`super::status::collect_untracked_and_ignored_with_rules_and_cancel`]) so results stay aligned with
//! `grit status` and system `git status`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};
use crate::porcelain::stash::remove_empty_dirs;
use crate::porcelain::status::{
    collect_untracked_and_ignored_inner, IgnoredMode, UntrackedCacheMissAction, UntrackedScan,
};
use crate::progress::{poll_cancel, CancelToken, ProgressSink};
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;

/// Which untracked/ignored paths [`clean_untracked`] may remove.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CleanScope {
    /// Untracked non-ignored paths only (default `git clean` without `-x`/`-X`).
    #[default]
    Untracked,
    /// Untracked and ignored paths (`git clean -x`).
    UntrackedAndIgnored,
    /// Ignored paths only (`git clean -X`).
    OnlyIgnored,
}

/// Inputs for [`clean_untracked`].
#[derive(Debug, Clone, Default)]
pub struct CleanOptions {
    /// Limit removal to paths matching these pathspecs (empty = entire tree).
    pub pathspecs: Vec<String>,
    /// When true, remove untracked directories as well as files (like `git clean -d`).
    pub directories: bool,
    /// When true, remove only files, not directories (like `git clean` without `-d`).
    pub files_only: bool,
    /// Which paths are eligible for removal.
    pub scope: CleanScope,
    /// Extra exclude patterns; matching paths are never removed (like `git clean -e`).
    pub excludes: Vec<String>,
    /// When true, report paths that would be removed without deleting anything.
    pub dry_run: bool,
}

fn clean_scope_includes_untracked(scope: CleanScope) -> bool {
    scope != CleanScope::OnlyIgnored
}

fn clean_scope_includes_ignored(scope: CleanScope) -> bool {
    matches!(
        scope,
        CleanScope::UntrackedAndIgnored | CleanScope::OnlyIgnored
    )
}

/// Result of [`clean_untracked`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CleanOutcome {
    /// Repository-relative paths removed (or that would be removed when `dry_run`).
    pub removed: Vec<String>,
}

/// Remove untracked paths from the work tree according to `opts`.
///
/// Nested repositories and submodule checkouts (directories containing `.git`) are
/// never entered or removed.
pub fn clean_untracked(
    repo: &Repository,
    opts: &CleanOptions,
    progress: &mut dyn ProgressSink,
) -> Result<CleanOutcome> {
    clean_untracked_with_cancel(repo, opts, progress, &CancelToken::never())
}

/// Like [`clean_untracked`], but stops between path removals when `cancel` is set.
///
/// Paths removed before cancellation stay removed; there is no index lock to restore.
pub fn clean_untracked_with_cancel(
    repo: &Repository,
    opts: &CleanOptions,
    progress: &mut dyn ProgressSink,
    cancel: &CancelToken,
) -> Result<CleanOutcome> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("clean requires a working tree".into()))?;

    let mut index = repo.load_index()?;
    let rules = WorktreeRules::from_repository(repo, &index)?;
    if opts.scope == CleanScope::OnlyIgnored && !opts.excludes.is_empty() {
        rules.ignore_mut().add_cli_excludes(&opts.excludes);
    }

    let remove_directories = opts.directories && !opts.files_only;
    let ignored_mode = if clean_scope_includes_ignored(opts.scope) {
        IgnoredMode::Traditional
    } else {
        IgnoredMode::No
    };
    // Match `git clean` without `-d`: never enumerate nested files inside untracked directories.
    let show_all = false;

    let clean_unit_dirs =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));
    let scan = UntrackedScan {
        ignored_mode,
        show_all,
        sort_paths: true,
        use_untracked_cache: false,
        cache_miss: UntrackedCacheMissAction::RefreshFull,
        clean_unit_dirs: Some(std::sync::Arc::clone(&clean_unit_dirs)),
    };

    let (untracked, ignored, collect_meta) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &opts.pathspecs,
        None,
        scan,
        &rules,
        None,
    )?;

    let mut candidates = Vec::new();
    if clean_scope_includes_untracked(opts.scope) {
        candidates.extend(untracked);
    }
    if clean_scope_includes_ignored(opts.scope) {
        candidates.extend(ignored);
    }
    if remove_directories && opts.scope != CleanScope::OnlyIgnored {
        let empty_dirs = empty_untracked_directories(
            repo,
            &index,
            &opts.pathspecs,
            &rules,
            opts.scope,
            &candidates,
        )?;
        candidates.extend(empty_dirs);
    }
    let pathspec_ctx = repo.pathspec_match_context();
    candidates.sort();
    candidates.dedup();
    let mut removed = Vec::new();
    for rel in candidates {
        if should_skip_nested_git(work_tree, &rel) {
            continue;
        }
        if !remove_directories && rel.ends_with('/') {
            continue;
        }
        let paths_to_add = if rel.ends_with('/') {
            let trimmed = rel.trim_end_matches('/');
            let abs = crate::repo_relpath::worktree_abs(work_tree, trimmed);
            if collect_meta.clean_collapsed_unit.contains(&rel) {
                vec![rel]
            } else if nested_git_worktree_descendant_under(&abs)
                || (!clean_scope_includes_ignored(opts.scope)
                    && dir_preserves_ignored_descendants(repo, &index, &rules, work_tree, &abs)?)
                || (!opts.excludes.is_empty()
                    && opts.scope != CleanScope::OnlyIgnored
                    && dir_has_excluded_descendant(work_tree, &abs, &opts.excludes, pathspec_ctx))
            {
                expand_collapsed_dir_paths(repo, &index, &rules, opts.scope, trimmed)?
            } else {
                vec![rel]
            }
        } else {
            vec![rel]
        };
        for path in paths_to_add {
            if should_skip_nested_git(work_tree, &path) {
                continue;
            }
            if !remove_directories && path.ends_with('/') {
                continue;
            }
            if !path_eligible_for_clean_removal(
                repo,
                &index,
                &rules,
                opts.scope,
                &path,
                &opts.excludes,
                pathspec_ctx,
            )? {
                continue;
            }
            if crate::worktree_cwd::cwd_would_be_removed_with_repo_path(
                work_tree,
                path.trim_end_matches('/'),
                repo.environment(),
            ) {
                return Err(Error::Message(format!(
                    "refusing to remove '{path}' because it contains the current directory"
                )));
            }
            removed.push(path);
        }
    }
    removed.sort();
    removed.dedup();

    if opts.dry_run {
        return Ok(CleanOutcome { removed });
    }

    let mut actually_removed = Vec::new();
    let environment = repo.environment();
    for rel in removal_order(&removed) {
        poll_cancel(progress, cancel)?;
        if remove_one_path(work_tree, &rel, remove_directories, environment)? {
            actually_removed.push(rel);
        }
    }
    actually_removed.sort();
    Ok(CleanOutcome {
        removed: actually_removed,
    })
}

/// Empty untracked directories are omitted by the status untracked walk but removed by `git clean -d`.
fn empty_untracked_directories(
    repo: &Repository,
    index: &Index,
    pathspecs: &[String],
    rules: &WorktreeRules,
    scope: CleanScope,
    already_listed: &[String],
) -> Result<Vec<String>> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("clean requires a working tree".into()))?;
    let listed: BTreeSet<String> = already_listed
        .iter()
        .map(|p| p.trim_end_matches('/').to_owned())
        .collect();
    let mut found = Vec::new();
    let entries: Vec<_> = fs::read_dir(work_tree)
        .map_err(Error::Io)?
        .filter_map(|e| e.ok())
        .collect();
    for entry in entries {
        let name = dir_entry_name_bytes(&entry);
        if name == b".git" {
            continue;
        }
        let child_abs = entry.path();
        if !child_abs.is_dir() {
            continue;
        }
        let stored_rel = crate::repo_relpath::strip_worktree_prefix_stored(&child_abs, work_tree)
            .unwrap_or_else(|| crate::repo_relpath::storage_string(&name));
        if let Some(path) = highest_empty_untracked_dir(
            repo,
            index,
            &child_abs,
            &stored_rel,
            pathspecs,
            rules,
            scope,
            &listed,
        )? {
            found.push(path);
        }
    }
    Ok(found)
}

fn path_matches_clean_exclude(
    path: &str,
    patterns: &[String],
    ctx: crate::pathspec::PathspecMatchContext,
) -> bool {
    if patterns.is_empty() {
        return false;
    }
    let trimmed = path.trim_end_matches('/');
    patterns.iter().any(|pat| {
        crate::pathspec::matches_pathspec_list_with_context(trimmed, std::slice::from_ref(pat), ctx)
            || crate::pathspec::matches_pathspec_list_with_context(
                &format!("{trimmed}/"),
                std::slice::from_ref(pat),
                ctx,
            )
    })
}

/// Whether `path` should be removed given scope and `-e` patterns (protection vs extra `-X` targets).
fn path_eligible_for_clean_removal(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    scope: CleanScope,
    path: &str,
    excludes: &[String],
    ctx: crate::pathspec::PathspecMatchContext,
) -> Result<bool> {
    let matches_exclude = path_matches_clean_exclude(path, excludes, ctx);
    match scope {
        CleanScope::OnlyIgnored => {
            if matches_exclude {
                return Ok(true);
            }
            let is_dir = path.ends_with('/');
            let check_path = path.trim_end_matches('/');
            let (ignored, _) =
                rules
                    .ignore_mut()
                    .check_path(repo, Some(index), check_path, is_dir)?;
            Ok(ignored)
        }
        _ => Ok(!matches_exclude),
    }
}

fn dir_has_excluded_descendant(
    work_tree: &Path,
    dir_abs: &Path,
    excludes: &[String],
    ctx: crate::pathspec::PathspecMatchContext,
) -> bool {
    walk_dir_has_excluded_descendant(work_tree, dir_abs, excludes, ctx)
}

fn walk_dir_has_excluded_descendant(
    work_tree: &Path,
    abs: &Path,
    excludes: &[String],
    ctx: crate::pathspec::PathspecMatchContext,
) -> bool {
    if is_nested_git_worktree(abs) {
        return false;
    }
    let entries: Vec<_> = match fs::read_dir(abs) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(_) => return false,
    };
    for entry in entries {
        let name = dir_entry_name_bytes(&entry);
        if name == b".git" {
            continue;
        }
        let path = entry.path();
        let rel = crate::repo_relpath::strip_worktree_prefix_stored(&path, work_tree)
            .unwrap_or_else(|| crate::repo_relpath::storage_string(&name));
        if path.is_dir() {
            if path_matches_clean_exclude(&format!("{rel}/"), excludes, ctx)
                || path_matches_clean_exclude(&rel, excludes, ctx)
            {
                return true;
            }
            if walk_dir_has_excluded_descendant(work_tree, &path, excludes, ctx) {
                return true;
            }
        } else if path_matches_clean_exclude(&rel, excludes, ctx) {
            return true;
        }
    }
    false
}

/// Returns the highest empty untracked directory marker under `abs` (e.g. `outer/` not `outer/inner/`).
#[allow(clippy::too_many_arguments)]
fn highest_empty_untracked_dir(
    repo: &Repository,
    index: &Index,
    abs: &Path,
    rel: &str,
    pathspecs: &[String],
    rules: &WorktreeRules,
    scope: CleanScope,
    listed: &BTreeSet<String>,
) -> Result<Option<String>> {
    if !rel.is_empty() && is_nested_git_worktree(abs) {
        return Ok(None);
    }
    if !rel.is_empty() && has_stage0_descendant_under_stored(index, rel) {
        return Ok(None);
    }
    if !rel.is_empty() && index_gitlink_at(index, rel) {
        return Ok(None);
    }

    let entries: Vec<_> = match fs::read_dir(abs) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(None),
    };

    let mut child_dirs = Vec::new();
    for entry in entries {
        let name = dir_entry_name_bytes(&entry);
        if name == b".git" {
            continue;
        }
        let child_abs = entry.path();
        if child_abs.is_dir() {
            let parent_bytes = crate::repo_relpath::storage_bytes(rel);
            let child_rel = crate::repo_relpath::storage_string(&crate::repo_relpath::join_rel(
                &parent_bytes,
                &name,
            ));
            child_dirs.push((child_rel, child_abs));
        } else if !rel.is_empty() {
            return Ok(None);
        }
    }

    if listed.contains(rel.trim_end_matches('/')) {
        return Ok(None);
    }

    if !crate::porcelain::status::status_path_matches(rel, pathspecs)
        && !crate::porcelain::status::status_path_matches(&format!("{rel}/"), pathspecs)
    {
        return Ok(None);
    }

    let (ignored, _) = rules
        .ignore_mut()
        .check_path(repo, Some(index), rel, true)?;
    if ignored && !clean_scope_includes_ignored(scope) {
        return Ok(None);
    }
    if scope == CleanScope::OnlyIgnored && child_dirs.is_empty() {
        return Ok(None);
    }

    if child_dirs.is_empty() {
        return Ok(Some(format!("{rel}/")));
    }

    for (child_rel, child_abs) in &child_dirs {
        if highest_empty_untracked_dir(
            repo, index, child_abs, child_rel, pathspecs, rules, scope, listed,
        )?
        .is_none()
        {
            return Ok(None);
        }
    }

    Ok(Some(format!("{rel}/")))
}

/// True when `dir_abs` contains ignored paths that `git clean` without `-x` would keep.
fn dir_preserves_ignored_descendants(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    work_tree: &Path,
    dir_abs: &Path,
) -> Result<bool> {
    walk_dir_preserves_ignored(repo, index, rules, work_tree, dir_abs)
}

fn walk_dir_preserves_ignored(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    work_tree: &Path,
    abs: &Path,
) -> Result<bool> {
    if is_nested_git_worktree(abs) {
        return Ok(false);
    }
    let entries: Vec<_> = match fs::read_dir(abs) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(false),
    };
    for entry in entries {
        let name = dir_entry_name_bytes(&entry);
        if name == b".git" {
            continue;
        }
        let path = entry.path();
        let is_dir = crate::read_dir_kind::entry_is_dir(&entry).unwrap_or(false);
        let rel = crate::repo_relpath::strip_worktree_prefix_stored(&path, work_tree)
            .unwrap_or_else(|| crate::repo_relpath::storage_string(&name));
        if is_dir {
            if is_nested_git_worktree(&path) {
                continue;
            }
            let (ignored, _) = rules
                .ignore_mut()
                .check_path(repo, Some(index), &rel, true)?;
            if ignored {
                return Ok(true);
            }
            if walk_dir_preserves_ignored(repo, index, rules, work_tree, &path)? {
                return Ok(true);
            }
        } else {
            let (ignored, _) = rules
                .ignore_mut()
                .check_path(repo, Some(index), &rel, false)?;
            if ignored {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn nested_git_worktree_descendant_under(dir_abs: &Path) -> bool {
    let entries = match fs::read_dir(dir_abs) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        if !crate::read_dir_kind::entry_is_dir(&entry).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        if is_nested_git_worktree(&path) {
            return true;
        }
        if nested_git_worktree_descendant_under(&path) {
            return true;
        }
    }
    false
}

/// Expand a collapsed untracked directory into the paths `git clean` would remove,
/// skipping nested repositories and (unless `include_ignored`) ignored files.
fn expand_collapsed_dir_paths(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    scope: CleanScope,
    dir_rel: &str,
) -> Result<Vec<String>> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| Error::Message("clean requires a working tree".into()))?;
    let mut paths = Vec::new();
    walk_expand_collapsed_dir(
        repo,
        index,
        rules,
        scope,
        &crate::repo_relpath::worktree_abs(work_tree, dir_rel),
        dir_rel,
        &mut paths,
    )?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

#[allow(clippy::too_many_arguments)]
fn walk_expand_collapsed_dir(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    scope: CleanScope,
    abs: &Path,
    rel: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    if is_nested_git_worktree(abs) {
        return Ok(());
    }
    let entries: Vec<_> = match fs::read_dir(abs) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(()),
    };

    let mut child_dirs = Vec::new();
    for entry in entries {
        let name = dir_entry_name_bytes(&entry);
        if name == b".git" {
            continue;
        }
        let child_abs = entry.path();
        let parent_bytes = crate::repo_relpath::storage_bytes(rel);
        let child_rel = crate::repo_relpath::storage_string(&crate::repo_relpath::join_rel(
            &parent_bytes,
            &name,
        ));
        if crate::read_dir_kind::entry_is_dir(&entry).unwrap_or(false) {
            child_dirs.push((child_rel, child_abs));
        } else if removable_untracked_path(repo, index, rules, scope, &child_rel)? {
            out.push(child_rel);
        }
    }

    for (child_rel, child_abs) in child_dirs {
        if is_nested_git_worktree(&child_abs) {
            continue;
        }
        let before = out.len();
        walk_expand_collapsed_dir(repo, index, rules, scope, &child_abs, &child_rel, out)?;
        if out.len() == before
            && empty_untracked_directory(&child_abs)
            && removable_untracked_path(repo, index, rules, scope, &format!("{child_rel}/"))?
        {
            out.push(format!("{child_rel}/"));
        }
    }
    Ok(())
}

fn empty_untracked_directory(abs: &Path) -> bool {
    fs::read_dir(abs)
        .map(|d| d.filter_map(|e| e.ok()).all(|e| e.file_name() == ".git"))
        .unwrap_or(false)
}

fn removable_untracked_path(
    repo: &Repository,
    index: &Index,
    rules: &WorktreeRules,
    scope: CleanScope,
    rel: &str,
) -> Result<bool> {
    if clean_scope_includes_ignored(scope) && scope == CleanScope::UntrackedAndIgnored {
        return Ok(true);
    }
    if scope == CleanScope::OnlyIgnored {
        let is_dir = rel.ends_with('/');
        let check_path = rel.trim_end_matches('/');
        let (ignored, _) = rules
            .ignore_mut()
            .check_path(repo, Some(index), check_path, is_dir)?;
        return Ok(ignored);
    }
    let is_dir = rel.ends_with('/');
    let check_path = rel.trim_end_matches('/');
    let (ignored, _) = rules
        .ignore_mut()
        .check_path(repo, Some(index), check_path, is_dir)?;
    Ok(!ignored)
}

fn index_gitlink_at(index: &Index, stored: &str) -> bool {
    let rel = crate::repo_relpath::storage_bytes(stored);
    index
        .find_entry_pos(&rel, 0)
        .is_some_and(|pos| index.entries()[pos].mode == MODE_GITLINK)
}

fn has_stage0_descendant_under_stored(index: &Index, stored: &str) -> bool {
    let dir = crate::repo_relpath::storage_bytes(stored);
    if dir.is_empty() {
        return index.has_stage0_descendant_under("");
    }
    if std::str::from_utf8(&dir).is_ok() && !stored.starts_with(crate::repo_relpath::STORAGE_PREFIX)
    {
        return index.has_stage0_descendant_under(stored);
    }
    let mut prefix = dir.into_owned();
    prefix.push(b'/');
    index
        .entries()
        .iter()
        .any(|e| e.stage() == 0 && e.path.starts_with(&prefix))
}

/// True when `abs` is a directory containing a nested `.git` (embedded repository).
pub(crate) fn is_nested_git_worktree(abs: &Path) -> bool {
    abs.join(".git").exists()
}

/// True when `rel` names a nested repository root or a path inside one (matches `git clean` / `git stash -u`).
pub(crate) fn path_in_nested_git_worktree(work_tree: &Path, rel: &str) -> bool {
    let trimmed = rel.trim_end_matches('/');
    if is_nested_git_worktree(&crate::repo_relpath::worktree_abs(work_tree, trimmed)) {
        return true;
    }
    let bytes = crate::repo_relpath::storage_bytes(trimmed);
    for (i, &byte) in bytes.iter().enumerate() {
        if byte != b'/' {
            continue;
        }
        let prefix = crate::repo_relpath::storage_string(&bytes[..i]);
        if is_nested_git_worktree(&crate::repo_relpath::worktree_abs(work_tree, &prefix)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

    fn stub_nested_git(dir: &Path) {
        let git = dir.join(".git");
        fs::create_dir_all(git.join("objects")).unwrap();
        fs::create_dir_all(git.join("refs/heads")).unwrap();
        fs::write(git.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        fs::write(
            git.join("config"),
            "[core]\n\trepositoryformatversion = 0\n\tbare = false\n",
        )
        .unwrap();
    }

    #[test]
    fn path_in_nested_git_worktree_uses_cumulative_byte_prefixes() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let work_tree = tmp.path();
        fs::create_dir_all(work_tree.join("container/nested")).unwrap();
        stub_nested_git(&work_tree.join("container/nested"));

        assert!(
            path_in_nested_git_worktree(work_tree, "container/nested/tracked.txt"),
            "paths under a multi-segment nested repo prefix must be recognized"
        );
        assert!(
            !path_in_nested_git_worktree(work_tree, "container/loose.txt"),
            "sibling paths outside the nested repo must not match"
        );
    }
}

fn should_skip_nested_git(work_tree: &Path, rel: &str) -> bool {
    path_in_nested_git_worktree(work_tree, rel)
}

fn removal_order(paths: &[String]) -> Vec<String> {
    let mut ordered = paths.to_vec();
    ordered.sort_by(|a, b| {
        let depth_a = a.matches('/').count() + if a.ends_with('/') { 1 } else { 0 };
        let depth_b = b.matches('/').count() + if b.ends_with('/') { 1 } else { 0 };
        depth_b.cmp(&depth_a).then_with(|| a.cmp(b))
    });
    ordered
}

fn remove_one_path(
    work_tree: &Path,
    rel: &str,
    remove_empty_parent_dirs: bool,
    environment: &crate::environment::Environment,
) -> Result<bool> {
    let stored = rel.trim_end_matches('/');
    let abs = crate::repo_relpath::worktree_abs(work_tree, stored);
    if !abs.exists() && abs.symlink_metadata().is_err() {
        return Err(Error::Message(format!(
            "failed to remove '{stored}': no such file or directory"
        )));
    }
    if rel.ends_with('/') || abs.is_dir() {
        if is_nested_git_worktree(&abs) {
            return Ok(false);
        }
        fs::remove_dir_all(&abs).map_err(Error::Io)?;
        return Ok(true);
    }
    fs::remove_file(&abs).map_err(Error::Io)?;
    if remove_empty_parent_dirs {
        if let Some(parent) = abs.parent() {
            remove_empty_dirs(parent, work_tree, environment);
        }
    }
    Ok(true)
}

#[cfg(unix)]
fn dir_entry_name_bytes(entry: &fs::DirEntry) -> Vec<u8> {
    use std::os::unix::ffi::OsStringExt;
    entry.file_name().into_vec()
}

#[cfg(not(unix))]
fn dir_entry_name_bytes(entry: &fs::DirEntry) -> Vec<u8> {
    entry
        .file_name()
        .to_string_lossy()
        .into_owned()
        .into_bytes()
}
