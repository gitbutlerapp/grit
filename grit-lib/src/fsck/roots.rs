//! Reachability root collection for fsck and garbage collection.

use crate::error::Result;
use crate::index::{CacheTreeNode, MODE_GITLINK};
use crate::objects::ObjectId;
use crate::reflog::{all_reflog_oids, reflog_oids_on_store};
use crate::refs;
use crate::repo::Repository;
use crate::state::HeadState;
use crate::worktree::list_worktrees;
use std::collections::HashSet;

/// Object ids that anchor connectivity and dangling-object classification.
///
/// Includes every ref tip, detached `HEAD` oids (repository and linked worktrees),
/// all reflog `old`/`new` oids from the repository ref store (including an injected
/// backend), plus linked-worktree admin-dir reflogs on disk,
/// index entries (all stages), cache-tree oids, and the stash ref when present.
///
/// # Errors
///
/// Propagates ref, reflog, index, or worktree enumeration failures.
pub fn reachability_roots(repo: &Repository) -> Result<Vec<ObjectId>> {
    let git_dir = repo.git_dir();
    let mut roots: HashSet<ObjectId> = HashSet::new();

    for (_, oid) in refs::list_refs_in(repo, "refs/")? {
        roots.insert(oid);
    }

    if let Ok(oid) = refs::resolve_ref_in(repo, "HEAD") {
        roots.insert(oid);
    }

    if let Ok(oid) = refs::resolve_ref_in(repo, "refs/stash") {
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

    roots.extend(reflog_oids_on_store(repo.refs())?);
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

    use std::sync::Arc;

    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::ident::Signature;
    use crate::maintenance::repository_fsck::{fsck, FsckOptions};
    use crate::objects::odb::{store::MemoryStore, OdbBuilder};
    use crate::objects::{HashAlgo, NewCommit, ObjectKind};
    use crate::porcelain::add::stage_for_commit;
    use crate::porcelain::commit::{create_commit, CommitRequest};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::refs::store::{
        Expected, MemoryRefStore, RawRef, RefTransaction, RefUpdate, ReflogUpdate,
    };
    use crate::repo::{InitOptions, RepositoryBuilder};
    use tempfile::TempDir;
    use time::OffsetDateTime;

    #[test]
    fn reachability_roots_includes_index_staged_blob() {
        let dir = TempDir::new().unwrap();
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        std::fs::write(repo.work_tree().unwrap().join("a"), b"x").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &crate::progress::NeverCancel).expect("stage");
        let ident = "T <t@example.com> 1 +0000".to_string();
        create_commit(
            &repo,
            &CommitRequest {
                message: "c".into(),
                author: Some(ident.parse().expect("signature")),
                committer: Some(ident.parse().expect("signature")),
                allow_empty: false,
                sign_override: None,
                amend: false,
                assume_worktree_synced: false,
                skip_pre_commit_tree_match: false,
                staged_change_count: None,
                ..Default::default()
            },
            &mut NullProgress,
            &crate::progress::NeverCancel,
        )
        .expect("commit");
        std::fs::write(repo.work_tree().unwrap().join("staged-only"), b"y").unwrap();
        stage_for_commit(&repo, &mut NullProgress, &crate::progress::NeverCancel).expect("stage2");
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

    #[test]
    fn injected_ref_store_reflog_commit_is_reachability_root() {
        let dir = TempDir::new().unwrap();
        let git_dir = dir.path().join("mem.git");
        std::fs::create_dir_all(&git_dir).unwrap();
        let repo = RepositoryBuilder::new(RepositoryOptions::empty())
            .odb_builder(
                OdbBuilder::files(git_dir.join("objects"))
                    .primary(Arc::new(MemoryStore::new(HashAlgo::Sha1)))
                    .alternates(false),
            )
            .ref_store(Arc::new(MemoryRefStore::new()))
            .initial_branch("main")
            .open(&git_dir, None)
            .unwrap();
        let sig = Signature::new(
            "T",
            "t@example.com",
            OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap(),
        )
        .unwrap();
        let tree = repo.odb().write(ObjectKind::Tree, &[]).unwrap();
        let c1 = repo
            .objects()
            .write_commit(&NewCommit::new(
                tree,
                sig.clone(),
                sig.clone(),
                "c1".to_string(),
            ))
            .unwrap();
        let c2 = repo
            .objects()
            .write_commit(&NewCommit::new(
                tree,
                sig.clone(),
                sig.clone(),
                "c2".to_string(),
            ))
            .unwrap();
        let ts = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let update = |new, exp, msg: &str| {
            let txn = RefTransaction::new()
                .update(RefUpdate {
                    name: "refs/heads/main".into(),
                    new_value: Some(RawRef::Direct(new)),
                    expected: exp,
                    flags: Default::default(),
                    reflog: Some(ReflogUpdate {
                        identity: "T <t@example.com>".into(),
                        message: msg.into(),
                        time: ts,
                        old_oid: None,
                        new_oid: None,
                    }),
                })
                .unwrap();
            repo.refs().prepare(txn).unwrap().commit().unwrap();
        };
        update(c1, Expected::Missing, "branch: Created");
        update(c2, Expected::Oid(c1), "reset: moving to c2");

        let roots = reachability_roots(&repo).unwrap();
        assert!(roots.contains(&c1), "reflog-only commit must be a root");

        let report = fsck(
            &repo,
            &FsckOptions::default().with_check_dangling(true),
            &mut NullProgress,
            &NeverCancel,
        )
        .unwrap();
        assert!(
            !report.dangling.iter().any(|(_, oid)| *oid == c1),
            "reflog-only commit must not be dangling: {:?}",
            report.dangling
        );
    }
}
