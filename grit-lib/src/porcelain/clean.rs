//! Remove untracked (and optionally ignored) paths from the work tree.
//!
//! [`clean_untracked`] reuses the same untracked/ignored walk as [`super::status`]
//! ([`super::status::collect_untracked_and_ignored_with_rules`]) so results stay aligned with
//! `grit status` and system `git status`.

use std::collections::BTreeSet;
use std::fs;
use std::path::Path;

use crate::error::{Error, Result};
use crate::index::{Index, MODE_GITLINK};
use crate::porcelain::stash::remove_empty_dirs;
use crate::porcelain::status::{collect_untracked_and_ignored_inner, IgnoredMode, UntrackedScan};
use crate::progress::ProgressSink;
use crate::repo::Repository;
use crate::worktree_rules::WorktreeRules;

/// Inputs for [`clean_untracked`].
#[derive(Debug, Clone, Default)]
pub struct CleanOptions {
    /// Limit removal to paths matching these pathspecs (empty = entire tree).
    pub pathspecs: Vec<String>,
    /// When true, remove untracked directories as well as files (like `git clean -d`).
    pub directories: bool,
    /// When true, also remove ignored paths (like `git clean -x`).
    pub include_ignored: bool,
    /// When true, report paths that would be removed without deleting anything.
    pub dry_run: bool,
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
        .work_tree
        .as_ref()
        .ok_or_else(|| Error::Message("clean requires a working tree".into()))?;

    let mut index = repo.load_index()?;
    let rules = WorktreeRules::from_repository(repo, &index)?;

    let ignored_mode = if opts.include_ignored {
        IgnoredMode::Traditional
    } else {
        IgnoredMode::No
    };
    let show_all = !opts.directories;

    let scan = UntrackedScan {
        ignored_mode,
        show_all,
        sort_paths: true,
        use_untracked_cache: false,
    };

    let (untracked, ignored) = collect_untracked_and_ignored_inner(
        repo,
        &mut index,
        work_tree,
        &opts.pathspecs,
        scan,
        &rules,
    )?;

    let mut candidates = untracked;
    if opts.include_ignored {
        candidates.extend(ignored);
    }
    if opts.directories {
        let empty_dirs = empty_untracked_directories(
            repo,
            &index,
            &opts.pathspecs,
            &rules,
            opts.include_ignored,
            &candidates,
        )?;
        candidates.extend(empty_dirs);
    }
    candidates.sort();
    candidates.dedup();

    let mut removed = Vec::new();
    for rel in candidates {
        if should_skip_nested_git(work_tree, &rel) {
            continue;
        }
        if !opts.directories && rel.ends_with('/') {
            continue;
        }
        let paths_to_add = if rel.ends_with('/') {
            let trimmed = rel.trim_end_matches('/');
            let abs = work_tree.join(trimmed);
            if nested_git_worktree_descendant_under(&abs) {
                expand_clean_paths_skipping_nested_repos(work_tree, trimmed)?
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
            if !opts.directories && path.ends_with('/') {
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
        if remove_one_path(work_tree, &rel, environment)? {
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
    include_ignored: bool,
    already_listed: &[String],
) -> Result<Vec<String>> {
    let work_tree = repo
        .work_tree
        .as_ref()
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
            repo,
            index,
            &child_abs,
            &name,
            pathspecs,
            rules,
            include_ignored,
            &listed,
        )? {
            found.push(path);
        }
    }
    Ok(found)
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
    include_ignored: bool,
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
    if ignored && !include_ignored {
        return Ok(None);
    }

    if child_dirs.is_empty() {
        return Ok(Some(format!("{rel}/")));
    }

    for (child_rel, child_abs) in &child_dirs {
        if highest_empty_untracked_dir(
            repo,
            index,
            child_abs,
            child_rel,
            pathspecs,
            rules,
            include_ignored,
            listed,
        )?
        .is_none()
        {
            return Ok(None);
        }
    }

    Ok(Some(format!("{rel}/")))
}

fn nested_git_worktree_descendant_under(dir_abs: &Path) -> bool {
    if !dir_abs.is_dir() {
        return false;
    }
    let entries = match fs::read_dir(dir_abs) {
        Ok(e) => e,
        Err(_) => return false,
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if is_nested_git_worktree(&path) {
            return true;
        }
        if nested_git_worktree_descendant_under(&path) {
            return true;
        }
    }
    false
}

/// When a collapsed directory contains a nested repository, enumerate removable paths
/// inside it without deleting nested-repo subtrees (matches `git clean` skipping).
fn expand_clean_paths_skipping_nested_repos(
    work_tree: &Path,
    dir_rel: &str,
) -> Result<Vec<String>> {
    let mut paths = Vec::new();
    walk_expand_clean(&work_tree.join(dir_rel), dir_rel, &mut paths)?;
    paths.sort();
    paths.dedup();
    Ok(paths)
}

fn walk_expand_clean(abs: &Path, rel: &str, out: &mut Vec<String>) -> Result<()> {
    if is_nested_git_worktree(abs) {
        return Ok(());
    }
    let entries: Vec<_> = match fs::read_dir(abs) {
        Ok(e) => e.filter_map(|e| e.ok()).collect(),
        Err(_) => return Ok(()),
    };
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
        if child_abs.is_dir() {
            if is_nested_git_worktree(&child_abs) {
                continue;
            }
            if nested_git_worktree_descendant_under(&child_abs) {
                walk_expand_clean(&child_abs, &child_rel, out)?;
            } else {
                out.push(format!("{child_rel}/"));
            }
        } else {
            out.push(child_rel);
        }
    }
    Ok(())
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

fn should_skip_nested_git(work_tree: &Path, rel: &str) -> bool {
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

fn is_nested_git_worktree(abs: &Path) -> bool {
    abs.is_dir() && abs.join(".git").exists()
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
    if let Some(parent) = abs.parent() {
        remove_empty_dirs(parent, work_tree, environment);
    }
    Ok(true)
}
