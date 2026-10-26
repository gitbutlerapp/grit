//! In a linked worktree of a reftable repository, `HEAD` lives in the worktree's stack and
//! branches in the common stack. An update through `HEAD` must write and log the branch in
//! the common stack, and log `HEAD` in the worktree stack.

use std::fs;
use std::ops::ControlFlow;
use std::path::{Path, PathBuf};

use grit_lib::objects::ObjectId;
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::reftable::ReftableStack;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags, ReflogUpdate,
    ReftableRefStore,
};
use grit_lib::repo::{InitOptions, Repository};

fn oid(byte: u8) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes[19] = byte;
    ObjectId::from_bytes(&bytes).expect("oid")
}

fn log(message: &str) -> Option<ReflogUpdate> {
    Some(ReflogUpdate {
        identity: "T <t@example.com>".to_owned(),
        message: message.to_owned(),
        time: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("time"),
        old_oid: None,
        new_oid: None,
    })
}

fn commit(store: &dyn RefStore, update: RefUpdate) {
    let txn = RefTransaction::new().update(update).expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
}

fn newest(store: &dyn RefStore, name: &str) -> Option<ReflogEntry> {
    let mut last = None;
    store
        .for_each_reflog_entry(name, true, &mut |e| {
            last = Some(e.clone());
            ControlFlow::Break(())
        })
        .expect("reflog");
    last
}

/// Common git dir with `main` at `oid(1)`, and a linked worktree `wt` on `main`.
fn linked_worktree(root: &Path) -> (PathBuf, PathBuf) {
    Repository::init(
        root,
        &InitOptions::new()
            .with_initial_branch("main")
            .with_ref_storage(grit_lib::refs::ref_storage::RefStorageFormat::Reftable),
    )
    .expect("init reftable");
    let common = root.join(".git").canonicalize().expect("common");
    let common_store = ReftableRefStore::open(common.clone()).expect("common store");
    commit(
        &common_store,
        RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: Some(RawRef::Direct(oid(1))),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        },
    );

    let wt_git_dir = common.join("worktrees").join("wt");
    fs::create_dir_all(wt_git_dir.join("reftable")).expect("worktree reftable");
    fs::write(wt_git_dir.join("reftable/tables.list"), "").expect("tables.list");
    fs::write(wt_git_dir.join("commondir"), "../..\n").expect("commondir");
    fs::write(
        wt_git_dir.join("gitdir"),
        root.join("wt/.git").display().to_string(),
    )
    .expect("gitdir");
    fs::write(wt_git_dir.join("HEAD"), "ref: refs/heads/.invalid\n").expect("HEAD stub");
    let wt_store = ReftableRefStore::open(wt_git_dir.clone()).expect("worktree store");
    commit(
        &wt_store,
        RefUpdate {
            name: "HEAD".into(),
            new_value: Some(RawRef::Symbolic("refs/heads/main".into())),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags {
                no_deref: true,
                log_only: false,
            },
        },
    );
    (common, wt_git_dir)
}

#[test]
fn head_update_in_linked_worktree_writes_and_logs_branch_in_common_stack() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (common, wt_git_dir) = linked_worktree(dir.path());
    let wt_store = ReftableRefStore::open(wt_git_dir.clone()).expect("worktree store");

    commit(
        &wt_store,
        RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(oid(2))),
            expected: Expected::Oid(oid(1)),
            reflog: log("commit: two"),
            flags: RefUpdateFlags::default(),
        },
    );

    assert_eq!(
        wt_store.read_raw("HEAD").expect("HEAD"),
        Some(RawRef::Symbolic("refs/heads/main".to_owned()))
    );
    let common_store = ReftableRefStore::open(common.clone()).expect("common store");
    assert_eq!(
        common_store.read_raw("refs/heads/main").expect("main"),
        Some(RawRef::Direct(oid(2)))
    );
    let wt_stack = ReftableStack::open(&wt_git_dir).expect("worktree stack");
    assert!(
        wt_stack
            .lookup_ref("refs/heads/main")
            .expect("lookup")
            .is_none(),
        "the branch must not be written into the worktree stack"
    );

    for (store, name) in [(&wt_store, "HEAD"), (&common_store, "refs/heads/main")] {
        let entry = newest(store, name).unwrap_or_else(|| panic!("{name} has no reflog"));
        assert_eq!((entry.old_oid, entry.new_oid), (oid(1), oid(2)), "{name}");
        assert_eq!(entry.message, "commit: two", "{name}");
    }
}

#[test]
fn stale_head_update_in_linked_worktree_is_rejected() {
    let dir = tempfile::tempdir().expect("tempdir");
    let (common, wt_git_dir) = linked_worktree(dir.path());
    let wt_store = ReftableRefStore::open(wt_git_dir).expect("worktree store");
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "HEAD".to_owned(),
            new_value: Some(RawRef::Direct(oid(3))),
            expected: Expected::Oid(oid(9)),
            reflog: log("stale"),
            flags: RefUpdateFlags::default(),
        })
        .expect("txn");
    assert!(wt_store.prepare(txn).is_err(), "stale compare-and-swap");
    let common_store = ReftableRefStore::open(common).expect("common store");
    assert_eq!(
        common_store.read_raw("refs/heads/main").expect("main"),
        Some(RawRef::Direct(oid(1)))
    );
}
