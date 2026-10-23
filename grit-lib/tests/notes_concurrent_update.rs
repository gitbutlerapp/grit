//! A notes update that races another writer must fail instead of dropping the other note.
//!
//! The ref store below moves the notes ref right after the writer first reads it, which is the
//! window a concurrent `grit notes add` or `git notes add` would hit.

use std::ops::ControlFlow;
use std::sync::{Arc, Mutex};

use grit_lib::config::environment::RepositoryOptions;
use grit_lib::error::Result;
use grit_lib::notes::NotesRef;
use grit_lib::objects::{ObjectId, ObjectKind};
use grit_lib::refs::commit_ref_update_on_store;
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::{
    Expected, MemoryRefStore, PreparedRefTransaction, RawRef, RefEntry, RefStorageFormat, RefStore,
    RefStoreError, RefTransaction, RefUpdate, RefUpdateFlags,
};
use grit_lib::repo::Repository;
use grit_lib::repo_handles::NotesWriteOptions;
use grit_test_support::git;

const NOTES: &str = "refs/notes/commits";

/// Memory ref store that, once armed, moves the notes ref to a given commit right after the
/// next read of it, simulating another writer landing between read and update.
#[derive(Debug, Default)]
struct RacingStore {
    inner: MemoryRefStore,
    move_to_after_read: Mutex<Option<ObjectId>>,
}

impl RacingStore {
    fn set(&self, oid: ObjectId) {
        commit_ref_update_on_store(
            &self.inner,
            RefUpdate {
                name: NOTES.to_owned(),
                new_value: Some(RawRef::Direct(oid)),
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            },
        )
        .expect("move notes ref");
    }

    fn arm(&self, oid: ObjectId) {
        *self
            .move_to_after_read
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(oid);
    }
}

impl RefStore for RacingStore {
    fn format(&self) -> RefStorageFormat {
        self.inner.format()
    }

    fn read_raw(&self, name: &str) -> Result<Option<RawRef>> {
        let value = self.inner.read_raw(name)?;
        if name == NOTES {
            let pending = self
                .move_to_after_read
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(oid) = pending {
                self.set(oid);
            }
        }
        Ok(value)
    }

    fn for_each_ref(
        &self,
        prefix: &str,
        f: &mut dyn FnMut(&RefEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.inner.for_each_ref(prefix, f)
    }

    fn prepare(
        &self,
        txn: RefTransaction,
    ) -> std::result::Result<Box<dyn PreparedRefTransaction + Send + '_>, RefStoreError> {
        self.inner.prepare(txn)
    }

    fn reflog_exists(&self, name: &str) -> Result<bool> {
        self.inner.reflog_exists(name)
    }

    fn for_each_reflog_entry(
        &self,
        name: &str,
        reverse: bool,
        f: &mut dyn FnMut(&ReflogEntry) -> ControlFlow<()>,
    ) -> Result<()> {
        self.inner.for_each_reflog_entry(name, reverse, f)
    }

    fn create_reflog(&self, name: &str) -> Result<()> {
        self.inner.create_reflog(name)
    }

    fn delete_reflog(&self, name: &str) -> Result<()> {
        self.inner.delete_reflog(name)
    }

    fn replace_reflog(&self, name: &str, entries: Vec<ReflogEntry>) -> Result<()> {
        self.inner.replace_reflog(name, entries)
    }

    fn for_each_reflog_ref(&self, f: &mut dyn FnMut(&str) -> ControlFlow<()>) -> Result<()> {
        self.inner.for_each_reflog_ref(f)
    }
}

fn opts() -> NotesWriteOptions {
    NotesWriteOptions {
        message: "notes".into(),
        committer: "Race <race@grit-scm.com> 1700000000 +0000".into(),
        author: None,
    }
}

/// Repository on an on-disk object store with the racing ref store, plus three annotated
/// objects; notes on the first two exist as consecutive tips `first` (a) and `second` (a, b),
/// and the ref is back at `first`.
struct Fixture {
    _dir: tempfile::TempDir,
    repo: Repository,
    store: Arc<RacingStore>,
    objects: [ObjectId; 3],
    second: ObjectId,
}

fn fixture() -> Fixture {
    let dir = tempfile::tempdir().expect("tempdir");
    git(dir.path(), &["init", "-q", "-b", "main", "."]);
    let store = Arc::new(RacingStore::default());
    let repo = Repository::open(
        &dir.path().join(".git"),
        &RepositoryOptions::empty().with_work_tree(dir.path()),
    )
    .expect("open")
    .with_ref_store(store.clone());
    let objects = ["a\n", "b\n", "c\n"].map(|body| {
        repo.odb()
            .write(ObjectKind::Blob, body.as_bytes())
            .expect("object")
    });
    let notes = repo.notes();
    notes
        .add(
            &NotesRef::commits(),
            &objects[0],
            b"note a\n",
            &opts(),
            false,
        )
        .expect("add a");
    let first = store.resolve(NOTES).expect("first tip");
    notes
        .add(
            &NotesRef::commits(),
            &objects[1],
            b"note b\n",
            &opts(),
            false,
        )
        .expect("add b");
    let second = store.resolve(NOTES).expect("second tip");
    store.set(first);
    Fixture {
        _dir: dir,
        repo,
        store,
        objects,
        second,
    }
}

fn annotated(fx: &Fixture) -> Vec<ObjectId> {
    let mut objects: Vec<ObjectId> = fx
        .repo
        .notes()
        .list(&NotesRef::commits(), None)
        .expect("list")
        .items
        .into_iter()
        .map(|n| n.object)
        .collect();
    objects.sort();
    objects
}

#[test]
fn add_racing_another_writer_fails_and_keeps_its_note() {
    let fx = fixture();
    fx.store.arm(fx.second);

    let result = fx.repo.notes().add(
        &NotesRef::commits(),
        &fx.objects[2],
        b"note c\n",
        &opts(),
        false,
    );

    assert!(
        result.is_err(),
        "add based on a stale notes tip must fail, got {result:?}"
    );
    assert_eq!(fx.store.resolve(NOTES).expect("tip"), fx.second);
    let mut expected = vec![fx.objects[0], fx.objects[1]];
    expected.sort();
    assert_eq!(
        annotated(&fx),
        expected,
        "the racing writer's note survives"
    );
}

#[test]
fn remove_racing_another_writer_fails_and_keeps_its_note() {
    let fx = fixture();
    fx.store.arm(fx.second);

    let result = fx
        .repo
        .notes()
        .remove_many(&NotesRef::commits(), &[fx.objects[0]], &opts());

    assert!(
        result.is_err(),
        "remove based on a stale notes tip must fail, got {result:?}"
    );
    assert_eq!(fx.store.resolve(NOTES).expect("tip"), fx.second);
    let mut expected = vec![fx.objects[0], fx.objects[1]];
    expected.sort();
    assert_eq!(annotated(&fx), expected);
}

#[test]
fn add_without_a_race_still_succeeds_on_the_same_store() {
    let fx = fixture();

    fx.repo
        .notes()
        .add(
            &NotesRef::commits(),
            &fx.objects[2],
            b"note c\n",
            &opts(),
            false,
        )
        .expect("add c");

    let mut expected = vec![fx.objects[0], fx.objects[2]];
    expected.sort();
    assert_eq!(annotated(&fx), expected);
}
