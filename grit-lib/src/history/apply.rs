//! Apply a [`RewritePlan`] atomically: refs, then index and worktree.

use std::fs;

use crate::config::ConfigSet;
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local, verify_tree_transition_allow_local,
};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::refs::ref_update_flags;
use crate::refs::should_autocreate_reflog_with_config;
use crate::refs::store::{
    Expected, RawRef, RefStoreError, RefTransaction, RefUpdate, ReflogUpdate,
};
use crate::repo::Repository;
use crate::state::resolve_head;

use super::error::HistoryError;
use super::types::{AppliedRewrite, ApplyOptions, PlannedRefUpdate, RewritePlan};

/// Commit ref movements from `plan`, then align index and worktree with the new `HEAD` tree when needed.
///
/// # Order
///
/// 1. When `plan.head` changes trees in a non-bare repo, verify local changes would not be
///    overwritten (untracked paths and staged/unstaged edits on tree-diff paths).
/// 2. One [`RefTransaction`] with compare-and-swap expectations for every planned ref update.
/// 3. Checkout from the old `HEAD` tree to the new one while preserving unrelated index/worktree edits.
///
/// Bare repositories skip step 1 and 3.
///
/// # Errors
///
/// Returns [`HistoryError::LocalChangesWouldBeOverwritten`], [`HistoryError::RefCasMismatch`],
/// [`HistoryError::RefApply`], or [`HistoryError::WorktreeApply`].
pub fn apply_rewrite(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
) -> Result<AppliedRewrite, HistoryError> {
    let (old_tree, new_tree) = head_tree_transition(plan);
    let needs_worktree = !repo.is_bare() && head_tree_changes(&old_tree, &new_tree);

    if let (Some(old), Some(new)) = (old_tree.as_ref(), new_tree.as_ref()) {
        if !repo.is_bare() {
            verify_worktree_ready_for_head_tree_change(repo, Some(old), new)?;
        }
    }

    let index_snapshot = if needs_worktree {
        fs::read(repo.index_path()).ok()
    } else {
        None
    };

    if !plan.ref_updates.is_empty() {
        commit_planned_ref_updates(repo, &plan.ref_updates, opts)?;
    }

    if needs_worktree {
        let (Some(old), Some(new)) = (old_tree, new_tree) else {
            return Err(HistoryError::WorktreeApply {
                message: "missing head tree for worktree apply".into(),
            });
        };
        if let Err(err) = apply_head_tree_to_worktree(repo, Some(&old), &new) {
            let _ = rollback_ref_updates(repo, &plan.ref_updates, opts);
            let _ = restore_worktree_after_failed_apply(
                repo,
                Some(&old),
                &new,
                index_snapshot.as_deref(),
            );
            return Err(err);
        }
    }

    let head = resolve_head_commit(repo)?;
    Ok(AppliedRewrite {
        ref_updates: plan.ref_updates.clone(),
        head,
    })
}

fn head_tree_transition(plan: &RewritePlan) -> (Option<ObjectId>, Option<ObjectId>) {
    (plan.head.old_tree, plan.head.new_tree)
}

fn head_tree_changes(old_tree: &Option<ObjectId>, new_tree: &Option<ObjectId>) -> bool {
    match (old_tree, new_tree) {
        (Some(old), Some(new)) => old != new,
        _ => false,
    }
}

fn verify_worktree_ready_for_head_tree_change(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<(), HistoryError> {
    prepare_tree_checkout(repo, from_tree, to_tree).map_err(map_prepare_checkout_error)?;
    if let Err(paths) = verify_tree_transition_allow_local(repo, from_tree, to_tree) {
        if paths.is_empty() {
            return Err(HistoryError::WorktreeApply {
                message: "worktree verification failed".into(),
            });
        }
        return Err(HistoryError::LocalChangesWouldBeOverwritten { paths });
    }
    Ok(())
}

fn map_prepare_checkout_error(err: crate::error::Error) -> HistoryError {
    if let Some(path) = err.to_string().strip_prefix("untracked file '") {
        if let Some(path) = path.strip_suffix("' would be overwritten — move or remove it first")
        {
            return HistoryError::LocalChangesWouldBeOverwritten {
                paths: vec![path.to_owned()],
            };
        }
    }
    HistoryError::WorktreeApply {
        message: err.to_string(),
    }
}

fn apply_head_tree_to_worktree(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<(), HistoryError> {
    checkout_tree_transition_allow_local(repo, from_tree, to_tree).map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })
}

fn commit_planned_ref_updates(
    repo: &Repository,
    updates: &[PlannedRefUpdate],
    opts: &ApplyOptions,
) -> Result<(), HistoryError> {
    let store = repo.refs();
    let git_dir = repo.git_dir();
    let config = repo.config().ok();
    let mut txn = RefTransaction::new();
    for planned in updates {
        let reflog = reflog_for_update(
            config.as_ref().map(|c| c.as_ref()),
            git_dir,
            &planned.name,
            opts,
        );
        let update = RefUpdate {
            name: planned.name.clone(),
            new_value: Some(RawRef::Direct(planned.new)),
            expected: Expected::Oid(planned.old),
            reflog,
            flags: ref_update_flags(&planned.name),
        };
        txn = txn
            .update(update)
            .map_err(|e| map_ref_store_error(e, &planned.name))?;
    }
    let prepared = store.prepare(txn).map_err(|e| {
        map_ref_store_error(
            e,
            updates.first().map(|u| u.name.as_str()).unwrap_or("refs"),
        )
    })?;
    prepared.commit().map_err(|e| {
        map_ref_store_error(
            e,
            updates.first().map(|u| u.name.as_str()).unwrap_or("refs"),
        )
    })?;
    Ok(())
}

fn rollback_ref_updates(
    repo: &Repository,
    updates: &[PlannedRefUpdate],
    opts: &ApplyOptions,
) -> Result<(), HistoryError> {
    if updates.is_empty() {
        return Ok(());
    }
    let inverse: Vec<PlannedRefUpdate> = updates
        .iter()
        .map(|u| PlannedRefUpdate {
            name: u.name.clone(),
            old: u.new,
            new: u.old,
        })
        .collect();
    commit_planned_ref_updates(repo, &inverse, opts)
}

fn reflog_for_update(
    config: Option<&ConfigSet>,
    git_dir: &std::path::Path,
    refname: &str,
    opts: &ApplyOptions,
) -> Option<ReflogUpdate> {
    let config = config?;
    if should_autocreate_reflog_with_config(config, git_dir, refname) {
        Some(ReflogUpdate {
            identity: opts.identity.clone(),
            message: opts.reflog_message.clone(),
            time: time::OffsetDateTime::now_utc(),
        })
    } else {
        None
    }
}

fn map_ref_store_error(err: RefStoreError, label: &str) -> HistoryError {
    match err {
        RefStoreError::ExpectedMismatch { name, .. } => HistoryError::RefCasMismatch { name },
        other => HistoryError::RefApply {
            message: format!("{label}: {other}"),
        },
    }
}

fn resolve_head_commit(repo: &Repository) -> Result<ObjectId, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(|e| HistoryError::RefApply {
        message: e.to_string(),
    })?;
    head.oid().copied().ok_or_else(|| HistoryError::RefApply {
        message: "HEAD has no commit".into(),
    })
}

fn restore_worktree_after_failed_apply(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    new_tree: &ObjectId,
    index_snapshot: Option<&[u8]>,
) -> Result<(), HistoryError> {
    checkout_between_trees(repo, Some(new_tree), old_tree.unwrap_or(new_tree)).map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })?;
    if let Some(bytes) = index_snapshot {
        fs::write(repo.index_path(), bytes).map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod inject_and_apply_tests {
    use std::fs;
    use std::ops::ControlFlow;
    use std::sync::Arc;

    use crate::config::environment::RepositoryOptions;
    use crate::history::{
        plan_rewrite, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
    };
    use crate::objects::{parse_commit, ObjectKind};
    use crate::reflog::ReflogEntry;
    use crate::refs::store::{
        MemoryRefStore, PreparedRefTransaction, RawRef, RefStore, RefStoreError, RefTransaction,
    };
    use crate::repo::{init_repository, Repository};
    use crate::RefStorageFormat;

    use super::*;

    #[derive(Debug)]
    struct FailOnPrepareStore {
        inner: MemoryRefStore,
    }

    impl RefStore for FailOnPrepareStore {
        fn format(&self) -> crate::refs::store::RefStorageFormat {
            self.inner.format()
        }

        fn read_raw(&self, name: &str) -> crate::error::Result<Option<RawRef>> {
            self.inner.read_raw(name)
        }

        fn for_each_ref(
            &self,
            prefix: &str,
            f: &mut dyn FnMut(&crate::refs::store::RefEntry) -> ControlFlow<()>,
        ) -> crate::error::Result<()> {
            self.inner.for_each_ref(prefix, f)
        }

        fn prepare(
            &self,
            _txn: RefTransaction,
        ) -> Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
            Err(RefStoreError::Corrupt("injected ref apply failure".into()))
        }

        fn reflog_exists(&self, name: &str) -> crate::error::Result<bool> {
            self.inner.reflog_exists(name)
        }

        fn for_each_reflog_entry(
            &self,
            name: &str,
            reverse: bool,
            f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
        ) -> crate::error::Result<()> {
            self.inner.for_each_reflog_entry(name, reverse, f)
        }

        fn create_reflog(&self, name: &str) -> crate::error::Result<()> {
            self.inner.create_reflog(name)
        }

        fn delete_reflog(&self, name: &str) -> crate::error::Result<()> {
            self.inner.delete_reflog(name)
        }

        fn replace_reflog(
            &self,
            name: &str,
            entries: Vec<ReflogEntry>,
        ) -> crate::error::Result<()> {
            self.inner.replace_reflog(name, entries)
        }

        fn for_each_reflog_ref(
            &self,
            f: &mut dyn FnMut(&str) -> ControlFlow<()>,
        ) -> crate::error::Result<()> {
            self.inner.for_each_reflog_ref(f)
        }
    }

    #[test]
    fn apply_rewrite_injected_ref_failure_restores_worktree_and_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
        let empty = repo.odb().write(ObjectKind::Tree, &[]).expect("tree");
        let blob = repo.odb().write(ObjectKind::Blob, b"file\n").expect("blob");
        let mut index = crate::index::Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: 5,
            oid: blob,
            flags: 8,
            flags_extended: None,
            path: b"file.txt".to_vec(),
            base_index_pos: 0,
        });
        let tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        fs::write(dir.path().join("file.txt"), "file\n").expect("write");
        let commit = crate::objects::serialize_commit(&crate::objects::CommitData {
            tree,
            parents: vec![],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: "root\n".into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let root = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &root).expect("ref");
        crate::refs::write_symbolic_ref(repo.git_dir(), "HEAD", "refs/heads/main").expect("head");

        let child_tree = crate::index::write_tree::write_tree_from_index(
            repo.odb(),
            &{
                let mut idx = index.clone();
                let blob2 = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
                idx.add_or_replace(crate::index::IndexEntry {
                    ctime_sec: 0,
                    ctime_nsec: 0,
                    mtime_sec: 0,
                    mtime_nsec: 0,
                    dev: 0,
                    ino: 0,
                    mode: crate::index::MODE_REGULAR,
                    uid: 0,
                    gid: 0,
                    size: 4,
                    oid: blob2,
                    flags: 8,
                    flags_extended: None,
                    path: b"two.txt".to_vec(),
                    base_index_pos: 0,
                });
                idx
            },
            "",
        )
        .expect("child tree");
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(child_tree),
                message: Some("rewritten\n".into()),
                author: None,
                parents: None,
            },
        );
        let plan = plan_rewrite(
            &repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".into(),
            },
        )
        .expect("plan");

        let store = Arc::new(FailOnPrepareStore {
            inner: MemoryRefStore::new(),
        });
        for update in &plan.ref_updates {
            store
                .inner
                .set_ref(&update.name, RawRef::Direct(update.old))
                .expect("seed");
        }
        store
            .inner
            .set_ref("HEAD", RawRef::Symbolic("refs/heads/main".to_owned()))
            .expect("head");
        let repo = repo.with_ref_store(store);
        let before_index = fs::read(repo.index_path()).expect("index");
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t>".into(),
                reflog_message: "rewrite".into(),
            },
        )
        .expect_err("inject");
        assert!(matches!(err, HistoryError::RefApply { .. }));
        assert_eq!(fs::read(repo.index_path()).expect("index"), before_index);
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );
        let head_tree = parse_commit(&repo.odb().read(&root).expect("read").data)
            .expect("parse")
            .tree;
        assert_eq!(
            parse_commit(
                &repo
                    .odb()
                    .read(&crate::refs::resolve_ref(repo.git_dir(), "HEAD").expect("head"))
                    .expect("read")
                    .data
            )
            .expect("parse")
            .tree,
            head_tree
        );
    }
}

#[cfg(test)]
mod ref_update_lines_tests {
    use super::*;
    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("oid")
    }

    #[test]
    fn ref_update_lines_match_update_ref_stdin_format() {
        let plan = RewritePlan {
            commits: Vec::new(),
            ref_updates: vec![PlannedRefUpdate {
                name: "refs/heads/main".into(),
                old: oid(1),
                new: oid(2),
            }],
            head: Default::default(),
        };
        assert_eq!(
            plan.ref_update_lines(),
            vec![format!(
                "update refs/heads/main {} {}",
                oid(2).to_hex(),
                oid(1).to_hex()
            )]
        );
    }
}
