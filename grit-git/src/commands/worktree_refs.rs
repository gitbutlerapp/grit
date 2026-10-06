//! Shared helpers for discovering branch refs occupied by worktrees.

use grit_lib::repo::Repository;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Return the canonical common git directory for this repository/worktree.
pub fn common_git_dir(repo: &Repository) -> PathBuf {
    if repo.git_dir.join("commondir").exists() {
        let common = fs::read_to_string(repo.git_dir.join("commondir")).unwrap_or_default();
        let common = common.trim();
        if common.is_empty() {
            return repo.git_dir.clone();
        }
        if Path::new(common).is_absolute() {
            PathBuf::from(common)
        } else {
            repo.git_dir
                .join(common)
                .canonicalize()
                .unwrap_or_else(|_| repo.git_dir.join(common))
        }
    } else {
        repo.git_dir.clone()
    }
}

/// Resolve the worktree path string from a worktree admin directory.
pub fn worktree_path_from_admin(admin_dir: &Path) -> String {
    if let Ok(gitdir_content) = fs::read_to_string(admin_dir.join("gitdir")) {
        let p = gitdir_content.trim().to_string();
        return Path::new(&p)
            .parent()
            .map(|parent| parent.display().to_string())
            .unwrap_or(p);
    }
    admin_dir.display().to_string()
}

fn main_worktree_path(repo: &Repository, common: &Path) -> String {
    let bare = common.join("HEAD").exists() && repo.work_tree.is_none();
    if bare {
        return common.display().to_string();
    }
    if let Some(wt) = &repo.work_tree {
        if repo.git_dir == common || !repo.git_dir.join("commondir").exists() {
            return wt.display().to_string();
        }
    }
    common
        .parent()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| common.display().to_string())
}

/// Build a map `refs/heads/<name>` -> worktree-path for all refs currently
/// occupied by the main worktree and linked worktrees.
///
/// Includes:
/// - branch checked out via `HEAD` symref
/// - branch in bisect state (`BISECT_START`)
/// - rebase refs from `rebase-apply/head-name` and `rebase-merge/head-name`
/// - `rebase-merge/onto` when it is itself a `refs/heads/*` ref name
pub fn occupied_branch_refs(repo: &Repository) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let common = common_git_dir(repo);
    let main_wt_path = main_worktree_path(repo, &common);

    // Bare main worktrees do not occupy branches (see `prepare_checked_out_branches` in git/branch.c).
    if !grit_lib::worktree::is_bare_repository(&common) {
        collect_from_admin(&common, &main_wt_path, &mut out);
    }

    let worktrees_dir = common.join("worktrees");
    if let Ok(entries) = fs::read_dir(&worktrees_dir) {
        for entry in entries.flatten() {
            let admin = entry.path();
            let wt_path = worktree_path_from_admin(&admin);
            collect_from_admin(&admin, &wt_path, &mut out);
        }
    }

    out
}

fn collect_from_admin(admin_dir: &Path, wt_path: &str, out: &mut HashMap<String, String>) {
    // HEAD symref (including symlink HEAD used in t2400 #17).
    if let Some(head_trimmed) = read_head_content(admin_dir) {
        if let Some(refname) = head_trimmed.strip_prefix("ref: ") {
            let refname = refname.trim();
            if refname.starts_with("refs/heads/") {
                out.insert(refname.to_string(), wt_path.to_string());
            }
        }
    }

    // Bisect state is stored in the common git dir; attribute it to a detached worktree.
    let head_is_detached = read_head_content(admin_dir).is_some_and(|h| {
        let h = h.trim();
        !h.starts_with("ref: ") && !h.is_empty()
    });
    if head_is_detached {
        let bisect_dir = common_git_dir_for_admin(admin_dir);
        if bisect_dir.join("BISECT_LOG").exists() {
            if let Ok(start) = fs::read_to_string(bisect_dir.join("BISECT_START")) {
                let trimmed = start.trim();
                if !trimmed.is_empty() {
                    let refname = if trimmed.starts_with("refs/heads/") {
                        trimmed.to_string()
                    } else if trimmed.starts_with("refs/") {
                        String::new()
                    } else {
                        format!("refs/heads/{trimmed}")
                    };
                    if !refname.is_empty() {
                        out.insert(refname, wt_path.to_string());
                    }
                }
            }
        }
    }

    // rebase-apply/head-name
    let rebase_apply = admin_dir.join("rebase-apply");
    if rebase_apply.exists() {
        if let Ok(head_name) = fs::read_to_string(rebase_apply.join("head-name")) {
            let refname = head_name.trim();
            if refname.starts_with("refs/heads/") {
                out.insert(refname.to_string(), wt_path.to_string());
            }
        }
    }

    // rebase-merge/head-name and onto
    let rebase_merge = admin_dir.join("rebase-merge");
    if rebase_merge.exists() {
        if let Ok(head_name) = fs::read_to_string(rebase_merge.join("head-name")) {
            let refname = head_name.trim();
            if refname.starts_with("refs/heads/") {
                out.insert(refname.to_string(), wt_path.to_string());
            }
        }
        if let Ok(onto) = fs::read_to_string(rebase_merge.join("onto")) {
            let onto = onto.trim();
            if onto.starts_with("refs/heads/") {
                out.insert(onto.to_string(), wt_path.to_string());
            }
        }
        if let Ok(update_refs) = fs::read_to_string(rebase_merge.join("update-refs")) {
            for line in update_refs.lines() {
                let refname = line.split_whitespace().next().unwrap_or("");
                if refname.starts_with("refs/heads/") {
                    out.insert(refname.to_string(), wt_path.to_string());
                }
            }
        }
    }
}

pub(crate) fn read_head_content(admin_dir: &Path) -> Option<String> {
    let head_path = admin_dir.join("HEAD");
    if head_path.is_symlink() {
        let target = fs::read_link(&head_path).ok()?;
        let s = target.to_string_lossy();
        Some(format!("ref: {s}"))
    } else {
        fs::read_to_string(&head_path).ok()
    }
}

fn common_git_dir_for_admin(admin_dir: &Path) -> PathBuf {
    if let Ok(common_raw) = fs::read_to_string(admin_dir.join("commondir")) {
        let common_rel = common_raw.trim();
        if common_rel.is_empty() {
            return admin_dir.to_path_buf();
        }
        if Path::new(common_rel).is_absolute() {
            PathBuf::from(common_rel)
        } else {
            admin_dir
                .join(common_rel)
                .canonicalize()
                .unwrap_or_else(|_| admin_dir.join(common_rel))
        }
    } else {
        admin_dir.to_path_buf()
    }
}

fn current_worktree_path(repo: &Repository) -> String {
    if let Some(wt) = &repo.work_tree {
        return wt.display().to_string();
    }
    worktree_path_from_admin(&repo.git_dir)
}

/// Branch `refs/heads/<branch_short>` occupied by any worktree (including rebase/bisect).
#[must_use]
pub fn branch_occupied_any_worktree(repo: &Repository, branch_short: &str) -> Option<String> {
    let target = format!("refs/heads/{branch_short}");
    occupied_branch_refs(repo).get(&target).cloned()
}

/// Compare worktree paths for equality (including canonical paths).
#[must_use]
pub fn worktree_paths_equal_pub(a: &str, b: &str) -> bool {
    worktree_paths_equal(a, b)
}

/// Path string for the worktree associated with `repo`.
#[must_use]
pub fn current_worktree_path_for_repo(repo: &Repository) -> String {
    current_worktree_path(repo)
}

fn worktree_paths_equal(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    Path::new(a)
        .canonicalize()
        .ok()
        .zip(Path::new(b).canonicalize().ok())
        .is_some_and(|(a, b)| a == b)
}

/// Like [`branch_occupied_any_worktree`], but ignores occupation by the current worktree.
#[must_use]
pub fn branch_occupied_by_other_worktree(repo: &Repository, branch_short: &str) -> Option<String> {
    let blocker = branch_occupied_any_worktree(repo, branch_short)?;
    if worktree_paths_equal(&blocker, &current_worktree_path(repo)) {
        None
    } else {
        Some(blocker)
    }
}

/// Branch held by another worktree only via in-progress rebase or bisect (detached HEAD).
#[must_use]
pub fn branch_held_by_rebase_or_bisect_elsewhere(
    repo: &Repository,
    branch_short: &str,
) -> Option<String> {
    let target = format!("refs/heads/{branch_short}");
    let occupied = occupied_branch_refs(repo);
    let blocker = occupied.get(&target)?;
    if worktree_paths_equal(blocker, &current_worktree_path(repo)) {
        return None;
    }
    let common = common_git_dir(repo);
    if head_ref_occupies_branch(&common, branch_short) {
        return None;
    }
    for entry in fs::read_dir(common.join("worktrees"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let admin = entry.path();
        if head_ref_occupies_branch(&admin, branch_short) {
            return None;
        }
    }
    Some(blocker.clone())
}

fn head_ref_occupies_branch(admin_dir: &Path, branch_short: &str) -> bool {
    let want = format!("refs/heads/{branch_short}");
    read_head_content(admin_dir).is_some_and(|h| {
        h.trim()
            .strip_prefix("ref: ")
            .is_some_and(|r| r.trim() == want)
    })
}
