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
use crate::progress::ProgressSink;
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
    _progress: &mut dyn ProgressSink,
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
            let abs = work_tree.join(trimmed);
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
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let child_abs = entry.path();
        if !child_abs.is_dir() {
            continue;
        }
        if let Some(path) = highest_empty_untracked_dir(
            repo, index, &child_abs, &name, pathspecs, rules, scope, &listed,
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
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let rel = crate::git_path::strip_worktree_prefix(&path, work_tree).unwrap_or(name);
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
    if !rel.is_empty() && has_tracked_under_index(index, rel) {
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
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let child_abs = entry.path();
        if child_abs.is_dir() {
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
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
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let path = entry.path();
        let is_dir = crate::read_dir_kind::entry_is_dir(&entry).unwrap_or(false);
        if is_dir {
            if is_nested_git_worktree(&path) {
                continue;
            }
            let rel = crate::git_path::strip_worktree_prefix(&path, work_tree).unwrap_or(name);
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
            let rel = crate::git_path::strip_worktree_prefix(&path, work_tree).unwrap_or(name);
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
        &work_tree.join(dir_rel),
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
        let name = entry.file_name().to_string_lossy().to_string();
        if name == ".git" {
            continue;
        }
        let child_abs = entry.path();
        let child_rel = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
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

fn has_tracked_under_index(index: &Index, rel_dir: &str) -> bool {
    let prefix = format!("{rel_dir}/");
    index
        .entries
        .iter()
        .any(|e| e.stage() == 0 && String::from_utf8_lossy(&e.path).starts_with(&prefix))
}

fn index_gitlink_at(index: &Index, rel: &str) -> bool {
    index.entries.iter().any(|e| {
        e.stage() == 0 && e.mode == MODE_GITLINK && String::from_utf8_lossy(&e.path) == rel
    })
}

/// True when `abs` is a directory containing a nested `.git` (embedded repository).
pub(crate) fn is_nested_git_worktree(abs: &Path) -> bool {
    abs.join(".git").exists()
}

/// True when `rel` names a nested repository root or a path inside one (matches `git clean` / `git stash -u`).
pub(crate) fn path_in_nested_git_worktree(work_tree: &Path, rel: &str) -> bool {
    let trimmed = rel.trim_end_matches('/');
    if is_nested_git_worktree(&work_tree.join(trimmed)) {
        return true;
    }
    let mut prefix = String::new();
    for part in trimmed.split('/') {
        if !prefix.is_empty() {
            prefix.push('/');
        }
        prefix.push_str(part);
        if is_nested_git_worktree(&work_tree.join(&prefix)) {
            return true;
        }
    }
    false
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
    let abs = work_tree.join(rel.trim_end_matches('/'));
    if !abs.exists() && abs.symlink_metadata().is_err() {
        return Ok(false);
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
