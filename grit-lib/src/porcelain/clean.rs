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
        if crate::worktree_cwd::cwd_would_be_removed_with_repo_path(
            work_tree,
            rel.trim_end_matches('/'),
            repo.environment(),
        ) {
            return Err(Error::Message(format!(
                "refusing to remove '{rel}' because it contains the current directory"
            )));
        }
        removed.push(rel);
    }

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
    walk_empty_untracked_dirs(
        repo,
        index,
        work_tree,
        "",
        pathspecs,
        rules,
        include_ignored,
        &listed,
        &mut found,
    )?;
    Ok(found)
}

#[allow(clippy::too_many_arguments)]
fn walk_empty_untracked_dirs(
    repo: &Repository,
    index: &Index,
    abs: &Path,
    rel: &str,
    pathspecs: &[String],
    rules: &WorktreeRules,
    include_ignored: bool,
    listed: &BTreeSet<String>,
    out: &mut Vec<String>,
) -> Result<()> {
    if !rel.is_empty() && is_nested_git_worktree(abs) {
        return Ok(());
    }
    if !rel.is_empty() && has_tracked_under_index(index, rel) {
        return Ok(());
    }
    if !rel.is_empty() && index_gitlink_at(index, rel) {
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
        if child_abs.is_dir() {
            let child_rel = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            child_dirs.push((child_rel, child_abs));
        } else if !rel.is_empty() {
            // Non-empty untracked directory; the status walk reports contents.
            return Ok(());
        }
    }

    for (child_rel, child_abs) in &child_dirs {
        walk_empty_untracked_dirs(
            repo,
            index,
            child_abs,
            child_rel,
            pathspecs,
            rules,
            include_ignored,
            listed,
            out,
        )?;
    }

    if rel.is_empty() {
        return Ok(());
    }

    if listed.contains(rel.trim_end_matches('/')) {
        return Ok(());
    }

    if !crate::porcelain::status::status_path_matches(rel, pathspecs)
        && !crate::porcelain::status::status_path_matches(&format!("{rel}/"), pathspecs)
    {
        return Ok(());
    }

    let (ignored, _) = rules
        .ignore_mut()
        .check_path(repo, Some(index), rel, true)?;
    if ignored && !include_ignored {
        return Ok(());
    }

    if child_dirs.is_empty() {
        out.push(format!("{rel}/"));
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
