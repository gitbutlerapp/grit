//! Git interoperability for [`grit_lib::refs::store::FilesRefStore`].

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, FilesRefStore, RawRef, RefStore, RefTransaction, RefUpdate, ReflogUpdate,
};
use std::collections::BTreeMap;
use std::fs;
use std::ops::ControlFlow;

mod support;
use support::{
    assert_fsck_when_git_interop, empty_commit_oid, files_repo, git, git_for_each_ref,
    git_interop_available, Backend,
};

fn sample_oid(byte: u8) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes[19] = byte;
    ObjectId::from_bytes(&bytes).expect("oid")
}

#[test]
fn files_store_git_roundtrip() {
    if !git_interop_available(Backend::Files) {
        eprintln!("skip files_store_git_roundtrip: git interop unavailable");
        return;
    }

    let repo = files_repo();
    let tip = empty_commit_oid(&repo);
    let git_dir = repo.git_dir();
    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");

    let log = ReflogUpdate {
        identity: "Store Test <store@test.example> 1700000000 +0000".to_owned(),
        message: "grit: batch".to_owned(),
        time: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("ts"),
    };

    let mut txn = RefTransaction::new();
    for i in 0..1000 {
        let name = if i == 0 {
            "refs/heads/main".to_owned()
        } else {
            format!("refs/heads/branch-{i:04}")
        };
        txn = txn
            .update(RefUpdate {
                name,
                new_value: Some(RawRef::Direct(tip)),
                expected: if i == 0 {
                    Expected::Any
                } else {
                    Expected::Missing
                },
                reflog: Some(log.clone()),
                flags: Default::default(),
            })
            .expect("push");
    }
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    assert_fsck_when_git_interop(&repo);

    let mut grit_map = BTreeMap::new();
    store
        .for_each_ref("refs/", &mut |entry| {
            let oid = match &entry.value {
                RawRef::Direct(o) => *o,
                RawRef::Symbolic(t) => store.resolve(t).expect("resolve"),
            };
            grit_map.insert(entry.name.clone(), oid);
            ControlFlow::Continue(())
        })
        .expect("iter");

    let git_map: BTreeMap<String, ObjectId> = git_for_each_ref(repo.worktree(), "refs/")
        .into_iter()
        .collect();
    assert_eq!(grit_map, git_map, "for-each-ref parity");

    let sample = "refs/heads/branch-0001";
    let git_log = git(
        repo.worktree(),
        &["reflog", "show", "--format=%gd %H", sample],
    );
    assert!(!git_log.trim().is_empty(), "git reflog for {sample}");

    git(
        repo.worktree(),
        &["update-ref", "refs/heads/from-git", &tip.to_hex()],
    );
    git(
        repo.worktree(),
        &["symbolic-ref", "refs/heads/sym-git", "refs/heads/from-git"],
    );

    let store2 = FilesRefStore::from_git_dir(&git_dir).expect("reopen");
    assert_eq!(
        store2.read_raw("refs/heads/from-git").expect("read"),
        Some(RawRef::Direct(tip))
    );
    assert_eq!(
        store2.read_raw("refs/heads/sym-git").expect("sym"),
        Some(RawRef::Symbolic("refs/heads/from-git".to_owned()))
    );
}

#[test]
fn files_store_respects_git_lock() {
    let repo = files_repo();
    let tip = empty_commit_oid(&repo);
    let git_dir = repo.git_dir();
    fs::write(git_dir.join("refs/heads/main"), format!("{tip}\n")).expect("write main");

    let lock_path = git_dir.join("refs/heads/locked.lock");
    fs::write(&lock_path, b"").expect("git lock");

    let store = FilesRefStore::from_git_dir(&git_dir).expect("store");
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/locked".to_owned(),
            new_value: Some(RawRef::Direct(sample_oid(9))),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        })
        .expect("txn");

    match store.prepare(txn) {
        Err(err) => assert!(
            matches!(err, grit_lib::refs::store::RefStoreError::LockHeld { .. }),
            "{err:?}"
        ),
        Ok(_) => panic!("expected LockHeld"),
    }

    let _ = fs::remove_file(lock_path);
}
