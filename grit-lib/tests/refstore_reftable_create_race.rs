//! ReftableRefStore create-only CAS under concurrent independent store handles.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use grit_lib::objects::ObjectId;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags, ReftableRefStore,
};
use grit_lib::repo::init_repository;

fn oid(byte: u8) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes[19] = byte;
    ObjectId::from_bytes(&bytes).expect("valid oid")
}

fn empty_reftable_git_dir() -> (tempfile::TempDir, std::path::PathBuf) {
    let root = tempfile::tempdir().expect("tempdir");
    init_repository(root.path(), false, "main", None, "reftable").expect("init");
    let git_dir = root.path().join(".git");
    let store = ReftableRefStore::open(git_dir.clone()).expect("open");
    let txn = RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: None,
            expected: Expected::Any,
            reflog: None,
            flags: RefUpdateFlags::default(),
        })
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
    let _ = std::fs::remove_file(git_dir.join("HEAD"));
    (root, git_dir)
}

fn missing_create_txn() -> RefTransaction {
    RefTransaction::new()
        .update(RefUpdate {
            name: "refs/heads/cas-race".to_owned(),
            new_value: Some(RawRef::Direct(oid(1))),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        })
        .expect("txn")
}

#[test]
fn concurrent_reftable_missing_create_single_winner_per_race() {
    let (_root, git_dir) = empty_reftable_git_dir();
    let store_a = Arc::new(ReftableRefStore::open(git_dir.clone()).expect("open a"));
    let store_b = Arc::new(ReftableRefStore::open(git_dir.clone()).expect("open b"));

    const RACES: usize = 100;
    let mut total_wins = 0usize;

    for _ in 0..RACES {
        let barrier = Arc::new(Barrier::new(2));
        let wins = Arc::new(AtomicUsize::new(0));

        let handles: Vec<_> = [store_a.clone(), store_b.clone()]
            .into_iter()
            .map(|store| {
                let barrier = barrier.clone();
                let wins = wins.clone();
                thread::spawn(move || {
                    barrier.wait();
                    let txn = missing_create_txn();
                    if store
                        .prepare(txn)
                        .ok()
                        .and_then(|p| p.commit().ok())
                        .is_some()
                    {
                        wins.fetch_add(1, Ordering::SeqCst);
                    }
                })
            })
            .collect();

        for handle in handles {
            handle.join().expect("join");
        }
        assert_eq!(
            wins.load(Ordering::SeqCst),
            1,
            "exactly one create-only winner per race"
        );
        total_wins += 1;

        let cleanup = RefTransaction::new()
            .update(RefUpdate {
                name: "refs/heads/cas-race".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
            .expect("cleanup txn");
        store_a
            .prepare(cleanup)
            .expect("cleanup prepare")
            .commit()
            .expect("cleanup commit");
    }

    assert_eq!(total_wins, RACES);
}
