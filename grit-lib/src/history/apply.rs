//! Apply a [`RewritePlan`] atomically: refs, then index and worktree.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::config::ConfigSet;
use crate::diff::diff_trees;
use crate::objects::ObjectId;
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local, verify_tree_transition_allow_local,
    TreeTransitionVerifyError,
};
use crate::porcelain::worktree_guard::prepare_tree_checkout;
use crate::refs::ref_update_flags;
use crate::refs::should_autocreate_reflog_with_config;
use crate::refs::store::{
    Expected, RawRef, RefStoreError, RefTransaction, RefUpdate, ReflogUpdate,
};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

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
/// Returns [`HistoryError::LocalChangesWouldBeOverwritten`], [`HistoryError::HeadMovedSincePlan`],
/// [`HistoryError::RefCasMismatch`], [`HistoryError::RefApply`], or [`HistoryError::WorktreeApply`].
pub fn apply_rewrite(
    repo: &Repository,
    plan: &RewritePlan,
    opts: &ApplyOptions,
) -> Result<AppliedRewrite, HistoryError> {
    verify_live_head_matches_plan(repo, &plan.head)?;

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
    let worktree_snapshot = if needs_worktree {
        match (old_tree.as_ref(), new_tree.as_ref()) {
            (Some(old), Some(new)) => Some(snapshot_worktree_for_tree_transition(repo, old, new)?),
            _ => None,
        }
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
        if let Err(checkout_err) = apply_head_tree_to_worktree(repo, Some(&old), &new) {
            let ref_rollback = rollback_ref_updates(repo, &plan.ref_updates, opts).err();
            let worktree_restore = restore_worktree_after_failed_apply(
                repo,
                index_snapshot.as_deref(),
                worktree_snapshot.as_ref(),
            )
            .err();
            return Err(apply_failure_after_ref_update(
                checkout_err,
                ref_rollback,
                worktree_restore,
            ));
        }
    }

    let head = resolve_head_commit(repo)?;
    Ok(AppliedRewrite {
        ref_updates: plan.ref_updates.clone(),
        head,
    })
}

fn verify_live_head_matches_plan(
    repo: &Repository,
    head: &super::types::HeadEffect,
) -> Result<(), HistoryError> {
    let Some(planned_commit) = head.old_commit else {
        return Ok(());
    };
    let live = resolve_head(repo.git_dir()).map_err(|e| HistoryError::RefApply {
        message: e.to_string(),
    })?;
    let mismatch = || HistoryError::HeadMovedSincePlan {
        planned_ref: head.head_ref.clone(),
        planned_commit,
    };
    match (&head.head_ref, live) {
        (Some(planned_ref), HeadState::Branch { refname, oid, .. }) => {
            if planned_ref != &refname {
                return Err(mismatch());
            }
            if oid.as_ref() != Some(&planned_commit) {
                return Err(mismatch());
            }
        }
        (None, HeadState::Detached { oid }) => {
            if oid != planned_commit {
                return Err(mismatch());
            }
        }
        (Some(_), HeadState::Detached { .. }) | (None, HeadState::Branch { .. }) => {
            return Err(mismatch());
        }
        (_, HeadState::Invalid) => return Err(mismatch()),
    }
    Ok(())
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
    match verify_tree_transition_allow_local(repo, from_tree, to_tree) {
        Ok(()) => Ok(()),
        Err(TreeTransitionVerifyError::WouldOverwrite(paths)) => {
            Err(HistoryError::LocalChangesWouldBeOverwritten { paths })
        }
        Err(TreeTransitionVerifyError::Failed(err)) => Err(map_prepare_checkout_error(err)),
    }
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

fn apply_failure_after_ref_update(
    checkout: HistoryError,
    ref_rollback: Option<HistoryError>,
    worktree_restore: Option<HistoryError>,
) -> HistoryError {
    let checkout_message = match checkout {
        HistoryError::WorktreeApply { message } => message,
        other => other.to_string(),
    };
    if ref_rollback.is_none() && worktree_restore.is_none() {
        return HistoryError::WorktreeApply {
            message: checkout_message,
        };
    }
    HistoryError::ApplyRollbackIncomplete {
        checkout: checkout_message,
        ref_rollback: ref_rollback.map(|e| e.to_string()),
        worktree_restore: worktree_restore.map(|e| e.to_string()),
    }
}

/// Pre-checkout bytes for one path in the forward tree transition (missing = absent).
#[derive(Debug, Clone, PartialEq, Eq)]
struct WorktreePathSnapshot {
    bytes: Option<Vec<u8>>,
    symlink: bool,
}

fn snapshot_worktree_for_tree_transition(
    repo: &Repository,
    from_tree: &ObjectId,
    to_tree: &ObjectId,
) -> Result<HashMap<String, WorktreePathSnapshot>, HistoryError> {
    let work_tree = repo
        .work_tree()
        .ok_or_else(|| HistoryError::WorktreeApply {
            message: "cannot snapshot worktree in a bare repository".into(),
        })?;
    let changes = diff_trees(repo.odb(), Some(from_tree), Some(to_tree), "").map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })?;
    let mut out = HashMap::new();
    for change in changes {
        let Some(path) = tree_transition_path(&change) else {
            continue;
        };
        out.insert(
            path.to_owned(),
            read_worktree_path_snapshot(work_tree, path),
        );
    }
    Ok(out)
}

fn tree_transition_path(change: &crate::diff::DiffEntry) -> Option<&str> {
    change.new_path.as_deref().or(change.old_path.as_deref())
}

fn read_worktree_path_snapshot(work_tree: &Path, path: &str) -> WorktreePathSnapshot {
    let abs = work_tree.join(path);
    let meta = match fs::symlink_metadata(&abs) {
        Ok(m) => m,
        Err(_) => {
            return WorktreePathSnapshot {
                bytes: None,
                symlink: false,
            };
        }
    };
    if meta.file_type().is_symlink() {
        let target = fs::read_link(&abs).ok();
        return WorktreePathSnapshot {
            bytes: target.map(|p| p.as_os_str().as_encoded_bytes().to_vec()),
            symlink: true,
        };
    }
    if meta.is_file() {
        return WorktreePathSnapshot {
            bytes: fs::read(&abs).ok(),
            symlink: false,
        };
    }
    WorktreePathSnapshot {
        bytes: None,
        symlink: false,
    }
}

fn restore_worktree_after_failed_apply(
    repo: &Repository,
    index_snapshot: Option<&[u8]>,
    worktree_snapshot: Option<&HashMap<String, WorktreePathSnapshot>>,
) -> Result<(), HistoryError> {
    if let Some(bytes) = index_snapshot {
        fs::write(repo.index_path(), bytes).map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
    }
    let Some(work_tree) = repo.work_tree() else {
        return Ok(());
    };
    let Some(snapshot) = worktree_snapshot else {
        return Ok(());
    };
    for (path, saved) in snapshot {
        let abs = work_tree.join(path);
        restore_worktree_path(&abs, saved).map_err(|e| HistoryError::WorktreeApply {
            message: format!("failed to restore '{path}': {e}"),
        })?;
    }
    Ok(())
}

fn restore_worktree_path(abs: &Path, saved: &WorktreePathSnapshot) -> std::io::Result<()> {
    if saved.bytes.is_none() {
        if abs.is_symlink() || abs.is_file() {
            fs::remove_file(abs)?;
        }
        return Ok(());
    }
    let Some(bytes) = saved.bytes.as_ref() else {
        return Ok(());
    };
    if saved.symlink {
        let _ = fs::remove_file(abs);
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            let target = String::from_utf8_lossy(bytes);
            if let Some(parent) = abs.parent() {
                fs::create_dir_all(parent)?;
            }
            symlink(target.as_ref(), abs)?;
        }
        #[cfg(not(unix))]
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "symlink restore requires unix",
            ));
        }
        return Ok(());
    }
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(abs, bytes)?;
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
    fn apply_rewrite_checkout_failure_rolls_back_refs_and_index() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
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
        let before_index = fs::read(repo.index_path()).expect("index");
        crate::repo::set_test_inject_index_write_fail(true);
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t>".into(),
                reflog_message: "rewrite".into(),
            },
        )
        .expect_err("checkout must fail after refs move");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(
            err,
            HistoryError::WorktreeApply { .. } | HistoryError::ApplyRollbackIncomplete { .. }
        ));
        assert_eq!(fs::read(repo.index_path()).expect("index"), before_index);
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );
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

    #[test]
    fn apply_rewrite_injected_checkout_failure_restores_worktree_and_index() {
        use crate::repo::set_test_inject_index_write_fail_remaining;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repository(dir.path(), false, "main", None, RefStorageFormat::Files)
            .expect("init");
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

        let blob2 = repo.odb().write(ObjectKind::Blob, b"two\n").expect("blob");
        let mut idx2 = index.clone();
        idx2.add_or_replace(crate::index::IndexEntry {
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
        let child_tree =
            crate::index::write_tree::write_tree_from_index(repo.odb(), &idx2, "").expect("tree");
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

        let before_index = fs::read(repo.index_path()).expect("index");
        set_test_inject_index_write_fail_remaining(1);
        let err = apply_rewrite(
            &repo,
            &plan,
            &ApplyOptions {
                identity: "T <t@t>".into(),
                reflog_message: "rewrite".into(),
            },
        )
        .expect_err("inject checkout failure");
        crate::repo::set_test_inject_index_write_fail(false);
        assert!(matches!(err, HistoryError::WorktreeApply { .. }));
        let tree_from_index_path = |path: &std::path::Path| {
            let index = crate::index::Index::load(path).expect("index");
            crate::index::write_tree::write_tree_from_index(repo.odb(), &index, "").expect("tree")
        };
        let before_index_path = dir.path().join(".git/index.before");
        fs::write(&before_index_path, &before_index).expect("snapshot");
        assert_eq!(
            tree_from_index_path(repo.index_path().as_path()),
            tree_from_index_path(&before_index_path)
        );
        assert_eq!(
            crate::refs::resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            root
        );
        assert!(dir.path().join("file.txt").exists());
        assert!(
            !dir.path().join("two.txt").exists(),
            "partial checkout must be rolled back"
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
