//! Ref view of a linked worktree whose repository keeps its refs in an injected store.

use std::io;
use std::ops::ControlFlow;
use std::path::PathBuf;
use std::sync::Arc;

use crate::error::{Error, Result};
use crate::reflog::ReflogEntry;
use crate::refs::{read_ref_file, Ref};

use super::{
    PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore, RefStoreError,
    RefTransaction, StoreResult,
};

/// A linked worktree's `HEAD`, read from `<admin>/HEAD`, over the repository's shared store.
///
/// An injected [`RefStore`] holds a single `HEAD`, the main worktree's, so a linked worktree's
/// own `HEAD` stays in its admin directory as Git lays it out. Every other ref, and every
/// reflog other than `HEAD`'s, comes from the shared store. `HEAD` is read-only here:
/// transactions and reflog writes that name it fail with [`LinkedWorktreeHeadReadOnly`].
#[derive(Debug)]
pub(crate) struct LinkedWorktreeRefStore {
    admin_dir: PathBuf,
    shared: Arc<dyn RefStore>,
}

/// A write to a linked worktree's `HEAD` through [`LinkedWorktreeRefStore`].
#[derive(Debug, thiserror::Error)]
#[error("HEAD of the linked worktree at {} cannot be written through an injected ref store", .admin_dir.display())]
pub(crate) struct LinkedWorktreeHeadReadOnly {
    admin_dir: PathBuf,
}

impl LinkedWorktreeRefStore {
    /// View of the linked worktree administered at `admin_dir` over `shared`.
    pub(crate) fn new(admin_dir: PathBuf, shared: Arc<dyn RefStore>) -> Self {
        Self { admin_dir, shared }
    }

    fn head_read_only(&self) -> RefStoreError {
        RefStoreError::backend(LinkedWorktreeHeadReadOnly {
            admin_dir: self.admin_dir.clone(),
        })
    }

    fn read_head(&self) -> Result<Option<RawRef>> {
        match read_ref_file(&self.admin_dir.join("HEAD")) {
            Ok(Ref::Direct(oid)) => Ok(Some(RawRef::Direct(oid))),
            Ok(Ref::Symbolic(target)) => Ok(Some(RawRef::Symbolic(target))),
            Err(Error::Io(err)) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }
}

impl RefStore for LinkedWorktreeRefStore {
    fn format(&self) -> RefStorageFormat {
        self.shared.format()
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        if name == "HEAD" {
            return self.read_head();
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
        if txn.updates().iter().any(|update| update.name == "HEAD") {
            return Err(self.head_read_only());
        }
        self.shared.prepare(txn)
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        if name == "HEAD" {
            return Ok(false);
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
            return Ok(());
        }
        self.shared.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        if name == "HEAD" {
            return Err(self.head_read_only().into());
        }
        self.shared.create_reflog(name)
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        if name == "HEAD" {
            return Err(self.head_read_only().into());
        }
        self.shared.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        if name == "HEAD" {
            return Err(self.head_read_only().into());
        }
        self.shared.replace_reflog(name, entries)
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        self.shared.for_each_reflog_ref(&mut |name| {
            if name == "HEAD" {
                ControlFlow::Continue(())
            } else {
                f(name)
            }
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use std::fs;

    use super::*;
    use crate::objects::ObjectId;
    use crate::refs::store::{Expected, MemoryRefStore, RefUpdate, RefUpdateFlags};

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
    fn head_writes_are_refused_and_branch_writes_reach_the_shared_store() {
        let (_dir, shared, view) = setup(Some("ref: refs/heads/linked\n"));
        let head = RefTransaction::new()
            .update(RefUpdate {
                name: "HEAD".into(),
                new_value: Some(RawRef::Direct(oid(4))),
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        let err = view.prepare(head).err().expect("HEAD is read-only");
        let RefStoreError::Backend(backend) = err else {
            panic!("expected a backend error, got {err:?}");
        };
        assert!(backend
            .downcast_ref::<LinkedWorktreeHeadReadOnly>()
            .is_some());
        assert!(view.create_reflog("HEAD").is_err());
        assert!(!view.reflog_exists("HEAD").expect("exists"));

        let branch = RefTransaction::new()
            .update(RefUpdate {
                name: "refs/heads/linked".into(),
                new_value: Some(RawRef::Direct(oid(5))),
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        view.prepare(branch)
            .expect("prepare")
            .commit()
            .expect("commit");
        assert_eq!(
            shared.read_raw("refs/heads/linked").expect("linked"),
            Some(RawRef::Direct(oid(5)))
        );
        assert_eq!(
            shared.read_raw("HEAD").expect("main HEAD"),
            Some(RawRef::Symbolic("refs/heads/main".into()))
        );
    }
}
