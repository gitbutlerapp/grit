//! Reachability root collection for fsck and garbage collection.

use crate::error::Result;
use crate::index::{CacheTreeNode, MODE_GITLINK};
use crate::objects::ObjectId;
use crate::reflog::all_reflog_oids;
use crate::refs::{self, Ref};
use crate::repo::Repository;
use crate::state::HeadState;
use crate::worktree::list_worktrees;
use std::collections::HashSet;

/// Object ids that anchor connectivity and dangling-object classification.
///
/// Includes every ref tip, detached `HEAD` oids (repository and linked worktrees),
/// all reflog `old`/`new` oids (main git dir and each linked worktree admin dir),
/// index entries (all stages), cache-tree oids, and the stash ref when present.
///
/// # Errors
///
/// Propagates ref, reflog, index, or worktree enumeration failures.
pub fn reachability_roots(repo: &Repository) -> Result<Vec<ObjectId>> {
    let git_dir = repo.git_dir();
    let mut roots: HashSet<ObjectId> = HashSet::new();

    for (_, oid) in refs::list_refs(git_dir, "refs/")? {
        roots.insert(oid);
    }

    let head_path = git_dir.join("HEAD");
    if head_path.is_file() {
        if let Ref::Direct(oid) = refs::read_ref_file(&head_path)? {
            roots.insert(oid);
        }
    }

    if let Ok(oid) = refs::resolve_ref(git_dir, "refs/stash") {
        roots.insert(oid);
    }

    let worktrees = list_worktrees(repo)?;
    for wt in &worktrees {
        match &wt.head {
            HeadState::Detached { oid } => {
                roots.insert(*oid);
            }
            HeadState::Branch { oid: Some(oid), .. } => {
                roots.insert(*oid);
            }
            HeadState::Branch { .. } | HeadState::Invalid => {}
        }
    }

    roots.extend(all_reflog_oids(git_dir)?);
    for wt in &worktrees {
        if wt.admin_dir != git_dir {
            roots.extend(all_reflog_oids(&wt.admin_dir)?);
        }
    }

    let index = repo.load_index()?;
    for entry in index.entries() {
        if entry.mode == MODE_GITLINK || entry.oid.is_zero() {
            continue;
        }
        roots.insert(entry.oid);
    }
    if let Some(root) = index.cache_tree_root {
        roots.insert(root);
    }
    if let Some(node) = &index.cache_tree {
        collect_cache_tree_oids(node, &mut roots);
    }

    Ok(roots.into_iter().collect())
}

/// Object ids recorded only in the index (and cache-tree), excluding gitlinks and zero ids.
///
/// Used to route missing-object diagnostics through the index fsck pass instead of the
/// connectivity walk (avoids duplicate `missingObject` reports for staged blobs).
pub fn index_object_roots(repo: &Repository) -> Result<HashSet<ObjectId>> {
    let mut out = HashSet::new();
    let index = repo.load_index()?;
    for entry in index.entries() {
        if entry.mode == MODE_GITLINK || entry.oid.is_zero() {
            continue;
        }
        out.insert(entry.oid);
    }
    if let Some(root) = index.cache_tree_root {
        out.insert(root);
    }
    if let Some(node) = &index.cache_tree {
        collect_cache_tree_oids(node, &mut out);
    }
    Ok(out)
}

fn collect_cache_tree_oids(node: &CacheTreeNode, out: &mut HashSet<ObjectId>) {
    if let Some(oid) = node.oid {
        out.insert(oid);
    }
    for child in &node.children {
        collect_cache_tree_oids(child, out);
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::NullProgress;
    use crate::repo::InitOptions;
    use tempfile::TempDir;

    #[test]
    fn reachability_roots_includes_index_staged_blob() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "c".into(),
                author: ident.clone(),
                committer: ident,
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
<<<<<<< New base: Merge branch 'factory/commit-dry-run-3068'
                ..Default::default()
||||||| Common ancestor
=======
                hooks: Default::default(),
>>>>>>> Current commit: commit: message file, author override, and no-verify hooks
            },
            &mut NullProgress,
        )
        .expect("commit");
        std::fs::write(repo.work_tree().unwrap().join("staged-only"), b"y").unwrap();
        stage_for_commit(&repo, &mut NullProgress).expect("stage2");
        let index = repo.load_index().expect("index");
        let staged = index
            .entries()
            .iter()
            .find(|e| e.path.ends_with(b"staged-only"))
            .expect("entry")
            .oid;
        let roots = reachability_roots(&repo).expect("roots");
        assert!(roots.contains(&staged));
    }
}
