//! Ref view of a linked worktree whose repository keeps its refs in an injected store.

use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::Result;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

use super::apply::{apply_update, ref_change_target, simulate_batch_apply, RefBatchState};
use super::{
    Expected, FilesRefStore, FilesRefStoreConfig, PreparedRefTransaction, RawRef, RefEntry,
    RefStorageFormat, RefStore, RefStoreError, RefTransaction, RefUpdate, RefUpdateFlags,
    ReflogUpdate, StoreResult,
};

/// A linked worktree's `HEAD` and `HEAD` reflog, kept in its admin directory, over the
/// repository's shared store.
///
/// An injected [`RefStore`] holds a single `HEAD`, the main worktree's, so a linked worktree's
/// own `HEAD` stays in its admin directory as Git lays it out: `<admin>/HEAD` and
/// `<admin>/logs/HEAD`. Every other ref, and every other reflog, lives in the shared store.
///
/// A transaction naming `HEAD` is split: the `HEAD` side (its value and reflog) is written to
/// the admin directory, everything else, including the branch a dereferencing `HEAD` update
/// moves, goes to the shared store. The shared batch commits first; the `HEAD` side commits
/// only when it succeeded.
#[derive(Debug)]
pub(crate) struct LinkedWorktreeRefStore {
    head: FilesRefStore,
    shared: Arc<dyn RefStore>,
}

impl LinkedWorktreeRefStore {
    /// View of the linked worktree administered at `admin_dir` over `shared`.
    pub(crate) fn new(admin_dir: PathBuf, shared: Arc<dyn RefStore>) -> Self {
        let head = FilesRefStore::open(FilesRefStoreConfig {
            git_dir: admin_dir.clone(),
            common_dir: admin_dir,
            namespace_prefix: None,
        });
        Self { head, shared }
    }

    /// Translate the transaction's `HEAD` update into the update applied to the admin
    /// directory and, when `HEAD` dereferences to a branch, the update that moves that branch
    /// in the shared store.
    ///
    /// Expectations are checked here with the usual semantics (an [`Expected::Oid`] on a
    /// dereferencing update compares the branch tip). The returned updates instead expect
    /// exactly the values observed, so a concurrent change between this read and the commit
    /// is caught by the locks of whichever side it touched.
    fn split_head_update(
        &self,
        update: RefUpdate,
    ) -> StoreResult<(Option<RefUpdate>, Option<RefUpdate>)> {
        let mut state = RefBatchState::default();
        let head = self.head.read_raw("HEAD").map_err(store_error)?;
        let mut next = match &head {
            Some(RawRef::Symbolic(target)) => Some(target.clone()),
            _ => None,
        };
        if let Some(value) = head {
            state.refs.insert("HEAD".to_owned(), value);
        }
        for _ in 0..SYMREF_MAXDEPTH {
            let Some(name) = next.take() else { break };
            if let Some(value) = self.shared.read_raw(&name).map_err(store_error)? {
                if let RawRef::Symbolic(target) = &value {
                    next = Some(target.clone());
                }
                state.refs.insert(name, value);
            }
        }

        simulate_batch_apply(&state, std::slice::from_ref(&update), &[], &[])?;
        let target = ref_change_target(&state, &update)?;
        let before = state.refs.clone();
        apply_update(&mut state, &update)?;
        let reflog = update.reflog.clone().and_then(|log| {
            let entry = state.reflogs.get("HEAD")?.last()?;
            Some(ReflogUpdate {
                old_oid: Some(entry.old_oid),
                new_oid: Some(entry.new_oid),
                ..log
            })
        });
        let observed = |name: &str| match before.get(name) {
            None => Expected::Missing,
            Some(RawRef::Direct(oid)) => Expected::Oid(*oid),
            Some(RawRef::Symbolic(target)) => Expected::Symref(target.clone()),
        };
        let head_expected = observed("HEAD");

        let Some(branch) = target.filter(|name| name != "HEAD") else {
            // `HEAD` itself changes, or only its reflog does.
            let log_only = update.flags.log_only;
            if log_only && reflog.is_none() {
                return Ok((None, None));
            }
            let head_update = RefUpdate {
                name: "HEAD".to_owned(),
                new_value: update.new_value,
                expected: head_expected,
                reflog,
                flags: RefUpdateFlags {
                    no_deref: true,
                    log_only,
                    ..Default::default()
                },
            };
            return Ok((Some(head_update), None));
        };

        let branch_logged = matches!(update.new_value, Some(RawRef::Direct(_)));
        let head_update = reflog.clone().map(|log| RefUpdate {
            name: "HEAD".to_owned(),
            new_value: log.new_oid.map(RawRef::Direct),
            expected: head_expected,
            reflog: Some(log),
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: true,
                ..Default::default()
            },
        });
        let branch_update = RefUpdate {
            expected: observed(&branch),
            name: branch,
            new_value: update.new_value,
            reflog: reflog.filter(|_| branch_logged),
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: false,
                ..Default::default()
            },
        };
        Ok((head_update, Some(branch_update)))
    }
}

fn store_error(err: crate::error::Error) -> RefStoreError {
    match err {
        crate::error::Error::RefStore(err) => err,
        other => RefStoreError::Corrupt(other.to_string()),
    }
}

/// The two halves of a split transaction: the shared store's batch and the admin
/// directory's `HEAD` batch.
struct SplitPrepared<'a> {
    shared: Option<Box<dyn PreparedRefTransaction + Send + 'a>>,
    head: Option<Box<dyn PreparedRefTransaction + Send + 'a>>,
}

impl PreparedRefTransaction for SplitPrepared<'_> {
    fn commit(self: Box<Self>) -> StoreResult<()> {
        let Self { shared, head } = *self;
        if let Some(shared) = shared {
            if let Err(err) = shared.commit() {
                if let Some(head) = head {
                    let _ = head.abort();
                }
                return Err(err);
            }
        }
        head.map_or(Ok(()), |head| head.commit())
    }

    fn abort(self: Box<Self>) -> StoreResult<()> {
        let Self { shared, head } = *self;
        let head = head.map_or(Ok(()), |head| head.abort());
        let shared = shared.map_or(Ok(()), |shared| shared.abort());
        head.and(shared)
    }
}

impl RefStore for LinkedWorktreeRefStore {
    fn format(&self) -> RefStorageFormat {
        self.shared.format()
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        if name == "HEAD" {
            return self.head.read_raw(name);
        }
        self.shared.read_raw(name)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.shared.for_each_ref(prefix, &mut |entry| {
            if entry.name == "HEAD" {
                ControlFlow::Continue(())
            } else {
                f(entry)
            }
        })
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> StoreResult<Box<dyn PreparedRefTransaction + Send + '_>> {
        if !txn.updates().iter().any(|update| update.name == "HEAD") {
            return self.shared.prepare(txn);
        }
        txn.validate()?;
        let (updates, reflog_renames, case_only_renames) = txn.into_parts();
        let mut shared_updates = Vec::with_capacity(updates.len());
        let mut head_update = None;
        for update in updates {
            if update.name == "HEAD" {
                let (head, branch) = self.split_head_update(update)?;
                head_update = head;
                shared_updates.extend(branch);
            } else {
                shared_updates.push(update);
            }
        }

        let mut shared_txn = RefTransaction::from_updates(shared_updates)?;
        for rename in reflog_renames {
            shared_txn = shared_txn.with_reflog_rename(rename);
        }
        for rename in case_only_renames {
            shared_txn = shared_txn.with_case_only_rename(rename);
        }
        // The admin directory's `HEAD` lock is this worktree's alone, while the shared store
        // may have one writer slot for every worktree: take the shared lock last so it is not
        // held through the `HEAD` side's file locking.
        let head = head_update
            .map(|update| {
                RefTransaction::from_updates(vec![update]).and_then(|txn| self.head.prepare(txn))
            })
            .transpose()?;
        let shared = if shared_txn == RefTransaction::new() {
            None
        } else {
            match self.shared.prepare(shared_txn) {
                Ok(prepared) => Some(prepared),
                Err(err) => {
                    if let Some(head) = head {
                        let _ = head.abort();
                    }
                    return Err(err);
                }
            }
        };
        Ok(Box::new(SplitPrepared { shared, head }))
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        if name == "HEAD" {
            return self.head.reflog_exists(name);
        }
        self.shared.reflog_exists(name)
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        if name == "HEAD" {
            return self.head.for_each_reflog_entry(name, reverse, f);
        }
        self.shared.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        if name == "HEAD" {
            return self.head.create_reflog(name);
        }
        self.shared.create_reflog(name)
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        if name == "HEAD" {
            return self.head.delete_reflog(name);
        }
        self.shared.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        if name == "HEAD" {
            return self.head.replace_reflog(name, entries);
        }
        self.shared.replace_reflog(name, entries)
    }

    fn rewrite_reflog(
        &self,
        name: &str,
        transform: &mut dyn FnMut(Vec<ReflogEntry>) -> Result<Vec<ReflogEntry>>,
    ) -> Result<()> {
        if name == "HEAD" {
            return self.head.rewrite_reflog(name, transform);
        }
        self.shared.rewrite_reflog(name, transform)
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        let mut names = Vec::new();
        self.shared.for_each_reflog_ref(&mut |name| {
            if name != "HEAD" {
                names.push(name.to_owned());
            }
            ControlFlow::Continue(())
        })?;
        if self.head.reflog_exists("HEAD")? {
            let at = names.partition_point(|name| name.as_str() < "HEAD");
            names.insert(at, "HEAD".to_owned());
        }
        for name in &names {
            if f(name).is_break() {
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::fs;

    use super::*;
    use crate::objects::ObjectId;
    use crate::refs::store::MemoryRefStore;

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(&[byte; 20]).expect("oid")
    }

    fn setup(
        head: Option<&str>,
    ) -> (
        tempfile::TempDir,
        Arc<MemoryRefStore>,
        LinkedWorktreeRefStore,
    ) {
        let dir = tempfile::tempdir().expect("tempdir");
        if let Some(head) = head {
            fs::write(dir.path().join("HEAD"), head).expect("HEAD");
        }
        let shared = Arc::new(MemoryRefStore::new());
        shared
            .set_ref("HEAD", RawRef::Symbolic("refs/heads/main".into()))
            .expect("main HEAD");
        shared
            .set_ref("refs/heads/main", RawRef::Direct(oid(1)))
            .expect("main");
        shared
            .set_ref("refs/heads/linked", RawRef::Direct(oid(2)))
            .expect("linked");
        let view = LinkedWorktreeRefStore::new(dir.path().to_path_buf(), shared.clone());
        (dir, shared, view)
    }

    fn log(message: &str) -> ReflogUpdate {
        ReflogUpdate {
            identity: "T <t@example.com> 1700000000 +0000".into(),
            message: message.into(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: None,
            new_oid: None,
        }
    }

    fn update(name: &str, value: RawRef, expected: Expected, flags: RefUpdateFlags) -> RefUpdate {
        RefUpdate {
            name: name.into(),
            new_value: Some(value),
            expected,
            reflog: None,
            flags,
        }
    }

    fn no_deref() -> RefUpdateFlags {
        RefUpdateFlags {
            no_deref: true,
            log_only: false,
            ..Default::default()
        }
    }

    fn commit(view: &LinkedWorktreeRefStore, updates: Vec<RefUpdate>) -> StoreResult<()> {
        view.prepare(RefTransaction::from_updates(updates)?)?
            .commit()
    }

    fn reflog(store: &dyn RefStore, name: &str) -> Vec<(ObjectId, ObjectId, String)> {
        let mut entries = Vec::new();
        store
            .for_each_reflog_entry(name, false, &mut |entry| {
                entries.push((entry.old_oid, entry.new_oid, entry.message.clone()));
                ControlFlow::Continue(())
            })
            .expect("reflog");
        entries
    }

    fn reflog_refs(store: &dyn RefStore) -> Vec<String> {
        let mut names = Vec::new();
        store
            .for_each_reflog_ref(&mut |name| {
                names.push(name.to_owned());
                ControlFlow::Continue(())
            })
            .expect("reflog refs");
        names
    }

    #[test]
    fn head_comes_from_the_admin_dir_and_branches_from_the_shared_store() {
        let (_dir, _shared, view) = setup(Some("ref: refs/heads/linked\n"));
        assert_eq!(
            view.read_raw("HEAD").expect("HEAD"),
            Some(RawRef::Symbolic("refs/heads/linked".into()))
        );
        assert_eq!(view.resolve("HEAD").expect("resolve"), oid(2));
        assert_eq!(view.resolve("refs/heads/main").expect("main"), oid(1));
        assert_eq!(view.format(), RefStorageFormat::Memory);

        let mut names = Vec::new();
        view.for_each_ref("", &mut |entry| {
            names.push(entry.name.clone());
            ControlFlow::Continue(())
        })
        .expect("list");
        assert_eq!(names, vec!["refs/heads/linked", "refs/heads/main"]);
    }

    #[test]
    fn detached_and_missing_head() {
        let (_dir, _shared, view) = setup(Some(&format!("{}\n", oid(3))));
        assert_eq!(
            view.read_raw("HEAD").expect("HEAD"),
            Some(RawRef::Direct(oid(3)))
        );
        let (_dir, _shared, view) = setup(None);
        assert_eq!(view.read_raw("HEAD").expect("HEAD"), None);
    }

    #[test]
    fn a_commit_moves_the_linked_branch_and_logs_head_in_the_admin_dir() {
        let (dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        assert!(!view.reflog_exists("HEAD").expect("exists"));
        for (old, new) in [(2, 5), (5, 6)] {
            let mut head = update(
                "HEAD",
                RawRef::Direct(oid(new)),
                Expected::Any,
                RefUpdateFlags {
                    no_deref: true,
                    log_only: true,
                    ..Default::default()
                },
            );
            head.reflog = Some(ReflogUpdate {
                old_oid: Some(oid(old)),
                new_oid: Some(oid(new)),
                ..log("commit")
            });
            let mut branch = update(
                "refs/heads/linked",
                RawRef::Direct(oid(new)),
                Expected::Oid(oid(old)),
                RefUpdateFlags::default(),
            );
            branch.reflog = head.reflog.clone();
            commit(&view, vec![head, branch]).expect("commit");
        }

        assert_eq!(
            shared.read_raw("refs/heads/linked").expect("linked"),
            Some(RawRef::Direct(oid(6)))
        );
        assert_eq!(
            shared.read_raw("refs/heads/main").expect("main"),
            Some(RawRef::Direct(oid(1)))
        );
        assert_eq!(
            shared.read_raw("HEAD").expect("main HEAD"),
            Some(RawRef::Symbolic("refs/heads/main".into()))
        );
        assert!(!shared.reflog_exists("HEAD").expect("shared HEAD reflog"));
        assert_eq!(
            fs::read_to_string(dir.path().join("HEAD")).expect("HEAD"),
            "ref: refs/heads/linked\n"
        );
        let expected = vec![
            (oid(2), oid(5), "commit".to_owned()),
            (oid(5), oid(6), "commit".to_owned()),
        ];
        assert!(view.reflog_exists("HEAD").expect("exists"));
        assert_eq!(reflog(&view, "HEAD"), expected);
        assert_eq!(reflog(shared.as_ref(), "refs/heads/linked"), expected);
        assert_eq!(
            fs::read_to_string(dir.path().join("logs/HEAD"))
                .expect("logs/HEAD")
                .lines()
                .count(),
            2
        );
        assert_eq!(reflog_refs(&view), vec!["HEAD", "refs/heads/linked"]);
    }

    #[test]
    fn detaching_rewrites_only_the_admin_head() {
        let (dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        let mut head = update(
            "HEAD",
            RawRef::Direct(oid(7)),
            Expected::Symref("refs/heads/linked".into()),
            no_deref(),
        );
        head.reflog = Some(ReflogUpdate {
            old_oid: Some(oid(2)),
            new_oid: Some(oid(7)),
            ..log("checkout: detach")
        });
        commit(&view, vec![head]).expect("detach");

        assert_eq!(
            fs::read_to_string(dir.path().join("HEAD")).expect("HEAD"),
            format!("{}\n", oid(7))
        );
        assert_eq!(
            view.read_raw("HEAD").expect("HEAD"),
            Some(RawRef::Direct(oid(7)))
        );
        assert_eq!(
            shared.read_raw("HEAD").expect("main HEAD"),
            Some(RawRef::Symbolic("refs/heads/main".into()))
        );
        assert_eq!(
            shared.read_raw("refs/heads/linked").expect("linked"),
            Some(RawRef::Direct(oid(2)))
        );
        assert_eq!(
            reflog(&view, "HEAD"),
            vec![(oid(2), oid(7), "checkout: detach".to_owned())]
        );

        let mut back = update(
            "HEAD",
            RawRef::Symbolic("refs/heads/main".into()),
            Expected::Oid(oid(7)),
            no_deref(),
        );
        back.reflog = Some(ReflogUpdate {
            old_oid: Some(oid(7)),
            new_oid: Some(oid(1)),
            ..log("checkout: main")
        });
        commit(&view, vec![back]).expect("switch");
        assert_eq!(view.resolve("HEAD").expect("HEAD"), oid(1));
        assert_eq!(
            reflog(&view, "HEAD")[1],
            (oid(7), oid(1), "checkout: main".to_owned())
        );
        assert!(!shared.reflog_exists("HEAD").expect("shared HEAD reflog"));
    }

    #[test]
    fn a_dereferencing_head_update_moves_the_linked_branch() {
        let (dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        let mut head = update(
            "HEAD",
            RawRef::Direct(oid(8)),
            Expected::Oid(oid(2)),
            RefUpdateFlags::default(),
        );
        head.reflog = Some(log("reset: moving"));
        commit(&view, vec![head.clone()]).expect("reset");

        assert_eq!(
            shared.read_raw("refs/heads/linked").expect("linked"),
            Some(RawRef::Direct(oid(8)))
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("HEAD")).expect("HEAD"),
            "ref: refs/heads/linked\n"
        );
        let expected = vec![(oid(2), oid(8), "reset: moving".to_owned())];
        assert_eq!(reflog(&view, "HEAD"), expected);
        assert_eq!(reflog(shared.as_ref(), "refs/heads/linked"), expected);
        assert!(!shared.reflog_exists("HEAD").expect("shared HEAD reflog"));

        let err = commit(&view, vec![head]).expect_err("stale expectation");
        assert!(
            matches!(err, RefStoreError::ExpectedMismatch { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_failed_shared_batch_leaves_the_admin_head_alone() {
        let (dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        let mut head = update("HEAD", RawRef::Direct(oid(9)), Expected::Any, no_deref());
        head.reflog = Some(log("detach"));
        let stale = update(
            "refs/heads/main",
            RawRef::Direct(oid(9)),
            Expected::Oid(oid(4)),
            RefUpdateFlags::default(),
        );
        let err = commit(&view, vec![head.clone(), stale]).expect_err("stale main");
        assert!(
            matches!(err, RefStoreError::ExpectedMismatch { .. }),
            "{err:?}"
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("HEAD")).expect("HEAD"),
            "ref: refs/heads/linked\n"
        );
        assert!(!dir.path().join("HEAD.lock").exists());
        assert!(!view.reflog_exists("HEAD").expect("exists"));
        assert_eq!(
            shared.read_raw("refs/heads/main").expect("main"),
            Some(RawRef::Direct(oid(1)))
        );

        let aborted = view
            .prepare(RefTransaction::from_updates(vec![head]).expect("txn"))
            .expect("prepare");
        assert!(dir.path().join("HEAD.lock").exists());
        aborted.abort().expect("abort");
        assert!(!dir.path().join("HEAD.lock").exists());
        assert_eq!(view.resolve("HEAD").expect("HEAD"), oid(2));
    }

    #[test]
    fn a_busy_shared_store_releases_the_admin_head_lock() {
        let (dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        let holder = shared
            .prepare(
                RefTransaction::from_updates(vec![update(
                    "refs/heads/main",
                    RawRef::Direct(oid(3)),
                    Expected::Any,
                    RefUpdateFlags::default(),
                )])
                .expect("txn"),
            )
            .expect("hold shared store");
        let head = update(
            "HEAD",
            RawRef::Direct(oid(4)),
            Expected::Oid(oid(2)),
            RefUpdateFlags::default(),
        );
        let err = commit(&view, vec![head.clone()]).expect_err("shared store busy");
        assert_eq!(
            err,
            RefStoreError::LockHeld {
                name: "refs/heads/linked".into()
            }
        );
        assert!(!dir.path().join("HEAD.lock").exists());
        holder.commit().expect("release");
        commit(&view, vec![head]).expect("commit once free");
        assert_eq!(view.resolve("HEAD").expect("HEAD"), oid(4));
    }

    #[test]
    fn head_reflog_maintenance_stays_in_the_admin_dir() {
        let (dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        view.create_reflog("HEAD").expect("create");
        assert!(dir.path().join("logs/HEAD").is_file());
        let entry = ReflogEntry {
            old_oid: oid(1),
            new_oid: oid(2),
            identity: "T <t@example.com> 1700000000 +0000".into(),
            message: "kept".into(),
        };
        view.replace_reflog("HEAD", vec![entry]).expect("replace");
        assert_eq!(
            reflog(&view, "HEAD"),
            vec![(oid(1), oid(2), "kept".to_owned())]
        );
        view.delete_reflog("HEAD").expect("delete");
        assert!(!view.reflog_exists("HEAD").expect("exists"));
        assert!(!shared.reflog_exists("HEAD").expect("shared HEAD reflog"));
    }
}
