//! Shared pathspec matching against index, HEAD tree, and worktree paths.

use crate::error::{Error, Result};
use crate::index::MODE_TREE;
use crate::objects::{parse_commit, parse_tree, ObjectId};
use crate::pathspec::{has_glob_chars, pathspec_is_exclude, sorted_paths_match_pathspec};
use crate::repo::Repository;
use crate::state::resolve_head;
use crate::unicode_normalization::resolve_worktree_path_for_staging;
use std::collections::HashSet;

/// Returns whether `resolved` matches at least one path in the index (stage 0), `HEAD` tree,
/// or an on-disk worktree file (literal pathspecs only).
///
/// # Errors
///
/// Propagates object-store failures while reading `HEAD`'s tree.
pub fn pathspec_matches_known_in_repository(repo: &Repository, resolved: &str) -> Result<bool> {
    pathspec_matches_known_in_paths(repo, resolved, &known_paths(repo)?)
}

/// Like [`pathspec_matches_known_in_repository`], but returns [`Error::PathspecNoMatch`] when
/// nothing matches.
pub fn ensure_pathspec_matches_known_in_repository(
    repo: &Repository,
    display_spec: &str,
    resolved: &str,
) -> Result<()> {
    if pathspec_matches_known_in_repository(repo, resolved)? {
        Ok(())
    } else {
        Err(Error::PathspecNoMatch {
            spec: display_spec.to_owned(),
        })
    }
}

fn pathspec_matches_known_in_paths(
    repo: &Repository,
    resolved: &str,
    known: &[String],
) -> Result<bool> {
    if sorted_paths_match_pathspec(resolved, known, repo.pathspec_match_context()) {
        return Ok(true);
    }
    if has_glob_chars(resolved) || resolved.starts_with(":(") {
        return Ok(false);
    }
    if pathspec_is_exclude(resolved) {
        return Ok(false);
    }
    if !resolved.starts_with(':') {
        if let Some(work_tree) = repo.work_tree() {
            let precompose = repo.pathspec_precompose_enabled();
            if precompose {
                let staged = resolve_worktree_path_for_staging(work_tree, resolved, true);
                if staged.abs.exists() {
                    return Ok(true);
                }
            } else if work_tree.join(resolved).exists() {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn known_paths(repo: &Repository) -> Result<Vec<String>> {
    let index = repo.load_index()?;
    let mut set = HashSet::<String>::new();
    for entry in &index.entries {
        if entry.stage() == 0 {
            set.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    set.extend(sorted_head_tree_paths(repo)?);
    let mut known: Vec<String> = set.into_iter().collect();
    known.sort_unstable();
    Ok(known)
}

/// Every non-tree path in `HEAD`'s tree, sorted in byte order (empty on an unborn branch).
///
/// # Errors
///
/// Propagates failures resolving `HEAD` or reading its commit and trees.
pub(crate) fn sorted_head_tree_paths(repo: &Repository) -> Result<Vec<String>> {
    let head = resolve_head(repo.git_dir())?;
    let Some(head_oid) = head.oid() else {
        return Ok(Vec::new());
    };
    let obj = repo.odb().read(head_oid)?;
    let commit = parse_commit(&obj.data)?;
    let mut paths = Vec::new();
    collect_tree_paths(repo, commit.tree, "", &mut paths)?;
    paths.sort_unstable();
    Ok(paths)
}

fn collect_tree_paths(
    repo: &Repository,
    tree_oid: ObjectId,
    prefix: &str,
    out: &mut Vec<String>,
) -> Result<()> {
    let obj = repo.odb().read(&tree_oid)?;
    for entry in parse_tree(&obj.data)? {
        let name = String::from_utf8_lossy(&entry.name);
        let path = if prefix.is_empty() {
            name.into_owned()
        } else {
            format!("{prefix}/{name}")
        };
        if entry.mode == MODE_TREE {
            collect_tree_paths(repo, entry.oid, &path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::repo::Repository;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn unknown_literal_pathspec_does_not_match_empty_repo() {
        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("discover");
        assert!(!pathspec_matches_known_in_repository(&repo, "does-not-exist").expect("check"));
    }

    #[test]
    fn literal_pathspec_matches_worktree_file() {
        let dir = TempDir::new().expect("tempdir");
        crate::repo::init_repository(
            dir.path(),
            false,
            "main",
            None,
            crate::RefStorageFormat::Files,
        )
        .expect("init");
        fs::write(dir.path().join("tracked.txt"), b"x").expect("write");
        let repo =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("discover");
        assert!(pathspec_matches_known_in_repository(&repo, "tracked.txt").expect("check"));
    }
}
