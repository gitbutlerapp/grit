//! Process current working directory relative to a Git work tree.
//!
//! Git refuses to remove a path when that removal would delete the process's
//! original working directory (`unpack-trees.c` `ERROR_CWD_IN_THE_WAY`): the
//! removed index path must **equal** the cwd or be a **parent prefix** of cwd
//! (removing a directory subtree that still contains cwd).
//!
//! Parent directory cleanup after removing a file must not `rmdir` an ancestor
//! that still contains cwd (matches leaving `foo` when cwd is `foo`).

use std::path::Path;

use crate::environment::Environment;

/// Normalize a repository-relative path to a POSIX-style string for comparisons.
fn normalize_repo_rel(path: &str) -> String {
    let mut s = path.trim().trim_start_matches('/').replace('\\', "/");
    while s.ends_with('/') && s.len() > 1 {
        s.pop();
    }
    s
}

/// Repository-relative path from `cwd` to `work_tree` root (POSIX, no leading `/`).
///
/// Tries canonical paths first, then a lexical `strip_prefix` when canonicalization
/// does not yield a prefix relationship (symlinks, `core.worktree` quirks).
#[must_use]
pub fn cwd_relative_under_work_tree(work_tree: &Path, cwd: &Path) -> Option<String> {
    cwd_relative_to_work_tree(work_tree, cwd)
}

fn cwd_relative_to_work_tree(work_tree: &Path, cwd: &Path) -> Option<String> {
    let rel = cwd
        .canonicalize()
        .ok()
        .zip(work_tree.canonicalize().ok())
        .and_then(|(c, w)| c.strip_prefix(&w).ok().map(|p| p.to_path_buf()))
        .or_else(|| cwd.strip_prefix(work_tree).ok().map(|p| p.to_path_buf()))?;
    let s = rel.to_string_lossy().replace('\\', "/");
    let s = s.trim_start_matches('/').to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Returns the repository-relative path of the current process directory under
/// `work_tree`, using canonical paths. Returns `None` when cwd is outside the
/// work tree, equals the work tree root, or cannot be resolved.
#[must_use]
pub fn process_cwd_repo_relative(work_tree: &Path, environment: &Environment) -> Option<String> {
    if let Some(prefix) = environment.git_prefix.as_deref() {
        let p = normalize_repo_rel(prefix);
        if !p.is_empty() {
            return Some(p);
        }
    }

    if let Some(inv) = environment.grit_invocation_cwd.as_deref() {
        let inv_path = Path::new(inv.trim());
        if let Some(rel) = cwd_relative_to_work_tree(work_tree, inv_path) {
            let n = normalize_repo_rel(&rel);
            if !n.is_empty() {
                return Some(n);
            }
        }
    }

    let cwd = environment.discovery_cwd();
    cwd_relative_to_work_tree(work_tree, &cwd)
}

/// Returns true if deleting the worktree entry at `repo_rel_path` (POSIX,
/// relative to the repository root) would delete the process cwd — i.e. the
/// path equals cwd or cwd lies under `repo_rel_path/`.
#[must_use]
pub fn cwd_would_be_removed_with_repo_path(
    work_tree: &Path,
    repo_rel_path: &str,
    environment: &Environment,
) -> bool {
    let Some(cwd_rel) = process_cwd_repo_relative(work_tree, environment) else {
        return false;
    };
    removal_path_covers_cwd(&normalize_repo_rel(repo_rel_path), &cwd_rel)
}

/// True when `removed` is the cwd path or a parent directory of cwd.
fn removal_path_covers_cwd(removed: &str, cwd_rel: &str) -> bool {
    if removed.is_empty() {
        return false;
    }
    cwd_rel == removed || cwd_rel.starts_with(&format!("{removed}/"))
}

/// Returns true if removing the directory `dir_abs` during empty-parent cleanup
/// would delete cwd (cwd is `dir` or a descendant of `dir`).
#[must_use]
pub fn cwd_would_be_removed_with_dir(work_tree: &Path, dir_abs: &Path, cwd_rel: &str) -> bool {
    let Ok(rel) = dir_abs.strip_prefix(work_tree) else {
        return false;
    };
    let dir_rel = normalize_repo_rel(rel.to_string_lossy().as_ref());
    if dir_rel.is_empty() {
        return false;
    }
    removal_path_covers_cwd(&dir_rel, cwd_rel)
}
