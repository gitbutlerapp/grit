//! Generic conformance tests for [`grit_lib::refs::store::RefStore`] backends.

use std::ops::ControlFlow;
use std::sync::Arc;
use std::thread;

use grit_lib::objects::ObjectId;
use grit_lib::reflog::ReflogEntry;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefStoreError, RefTransaction, RefUpdate, RefUpdateFlags,
    ReflogUpdate,
};

type StoreFactory = Arc<dyn Fn() -> Arc<dyn RefStore> + Send + Sync>;

fn oid(byte: u8) -> ObjectId {
    let mut bytes = [0u8; 20];
    bytes[19] = byte;
    ObjectId::from_bytes(&bytes).expect("valid oid")
}

fn sample_log() -> ReflogUpdate {
    ReflogUpdate {
        identity: "Test User <test@example.com>".to_owned(),
        message: "commit: msg".to_owned(),
        time: time::OffsetDateTime::from_unix_timestamp(1_700_000_000).expect("valid timestamp"),
    }
}

fn update(name: &str, new_value: Option<RawRef>, expected: Expected) -> RefUpdate {
    RefUpdate {
        name: name.to_owned(),
        new_value,
        expected,
        reflog: None,
        flags: RefUpdateFlags::default(),
    }
}

fn seed_direct(store: &dyn RefStore, name: &str, value: ObjectId) {
    let txn = RefTransaction::new()
        .update(update(name, Some(RawRef::Direct(value)), Expected::Missing))
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
}

fn seed_symref(store: &dyn RefStore, name: &str, target: &str) {
    let txn = RefTransaction::new()
        .update(update(
            name,
            Some(RawRef::Symbolic(target.to_owned())),
            Expected::Missing,
        ))
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
}

fn prepare_err(store: &dyn RefStore, txn: RefTransaction) -> RefStoreError {
    match store.prepare(txn) {
        Err(err) => err,
        Ok(_) => panic!("expected prepare to fail"),
    }
}

pub fn run_refstore_conformance(factory: StoreFactory) {
    cas_success(&factory);
    cas_failure(&factory);
    create_only_missing(&factory);
    delete_missing_ref(&factory);
    df_conflict_in_batch(&factory);
    df_conflict_against_store(&factory);
    symref_deref_reflog(&factory);
    symref_no_deref_reflog(&factory);
    prefix_iteration_order(&factory);
    abort_leaves_state_unchanged(&factory);
    concurrent_prepare_lock_held(&factory);
    reflog_roundtrip(&factory);
}

fn cas_success(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/main", oid(1));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/main",
            Some(RawRef::Direct(oid(2))),
            Expected::Oid(oid(1)),
        ))
        .expect("txn");
    let prepared = store.prepare(txn).expect("prepare");
    prepared.commit().expect("commit");
    assert_eq!(
        store.read_raw("refs/heads/main").expect("read"),
        Some(RawRef::Direct(oid(2)))
    );
}

fn cas_failure(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/x", oid(1));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/x",
            Some(RawRef::Direct(oid(2))),
            Expected::Oid(oid(9)),
        ))
        .expect("txn");
    let err = prepare_err(store.as_ref(), txn);
    assert!(matches!(err, RefStoreError::ExpectedMismatch { .. }));
    assert_eq!(
        store.read_raw("refs/heads/x").expect("read"),
        Some(RawRef::Direct(oid(1)))
    );
}

fn create_only_missing(factory: &StoreFactory) {
    let store = factory();
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/new",
            Some(RawRef::Direct(oid(3))),
            Expected::Missing,
        ))
        .expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
    assert_eq!(
        store.read_raw("refs/heads/new").expect("read"),
        Some(RawRef::Direct(oid(3)))
    );

    let txn2 = RefTransaction::new()
        .update(update(
            "refs/heads/new",
            Some(RawRef::Direct(oid(4))),
            Expected::Missing,
        ))
        .expect("txn");
    assert!(matches!(
        prepare_err(store.as_ref(), txn2),
        RefStoreError::ExpectedMismatch { .. }
    ));
}

fn delete_missing_ref(factory: &StoreFactory) {
    let store = factory();
    let txn = RefTransaction::new()
        .update(update("refs/heads/gone", None, Expected::Exists))
        .expect("txn");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::ExpectedMismatch { .. }
    ));
}

fn df_conflict_in_batch(factory: &StoreFactory) {
    let store = factory();
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/p",
            Some(RawRef::Direct(oid(1))),
            Expected::Missing,
        ))
        .expect("first")
        .update(update(
            "refs/heads/p/child",
            Some(RawRef::Direct(oid(2))),
            Expected::Missing,
        ))
        .expect("second");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::NameUnavailable { .. }
    ));
}

fn df_conflict_against_store(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/parent", oid(1));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/parent/leaf",
            Some(RawRef::Direct(oid(2))),
            Expected::Missing,
        ))
        .expect("txn");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::NameUnavailable { .. }
    ));
}

fn symref_deref_reflog(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/target", oid(10));
    seed_symref(store.as_ref(), "refs/heads/sym", "refs/heads/target");

    let mut upd = update(
        "refs/heads/sym",
        Some(RawRef::Symbolic("refs/heads/target".to_owned())),
        Expected::Any,
    );
    upd.reflog = Some(sample_log());
    let txn = RefTransaction::new().update(upd).expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    let mut last: Option<ReflogEntry> = None;
    store
        .for_each_reflog_entry("refs/heads/sym", true, &mut |e| {
            last = Some(e.clone());
            ControlFlow::Break(())
        })
        .expect("reflog");
    let entry = last.expect("entry");
    assert_eq!(entry.old_oid, oid(10));
    assert_eq!(entry.new_oid, oid(10));
}

fn symref_no_deref_reflog(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/t2", oid(11));
    seed_symref(store.as_ref(), "refs/heads/s2", "refs/heads/t2");

    let mut upd = update(
        "refs/heads/s2",
        Some(RawRef::Symbolic("refs/heads/t2".to_owned())),
        Expected::Any,
    );
    upd.reflog = Some(sample_log());
    upd.flags.no_deref = true;
    let txn = RefTransaction::new().update(upd).expect("txn");
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");

    let mut last: Option<ReflogEntry> = None;
    store
        .for_each_reflog_entry("refs/heads/s2", true, &mut |e| {
            last = Some(e.clone());
            ControlFlow::Break(())
        })
        .expect("reflog");
    let entry = last.expect("entry");
    assert!(entry.old_oid.is_zero());
    assert!(entry.new_oid.is_zero());
}

fn prefix_iteration_order(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/b", oid(1));
    seed_direct(store.as_ref(), "refs/heads/a", oid(1));
    seed_direct(store.as_ref(), "refs/tags/v1", oid(1));

    let mut names = Vec::new();
    store
        .for_each_ref("refs/heads/", &mut |entry| {
            names.push(entry.name.clone());
            ControlFlow::Continue(())
        })
        .expect("iter");
    assert_eq!(names, vec!["refs/heads/a", "refs/heads/b"]);

    let mut limited = Vec::new();
    store
        .for_each_ref("refs/heads/a", &mut |entry| {
            limited.push(entry.name.clone());
            ControlFlow::Continue(())
        })
        .expect("iter prefix boundary");
    assert_eq!(limited, vec!["refs/heads/a"]);
}

fn abort_leaves_state_unchanged(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/stay", oid(5));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/stay",
            Some(RawRef::Direct(oid(6))),
            Expected::Any,
        ))
        .expect("txn");
    let prepared = store.prepare(txn).expect("prepare");
    prepared.abort().expect("abort");
    assert_eq!(
        store.read_raw("refs/heads/stay").expect("read"),
        Some(RawRef::Direct(oid(5)))
    );
}

fn concurrent_prepare_lock_held(factory: &StoreFactory) {
    let store = factory();
    seed_direct(store.as_ref(), "refs/heads/c", oid(1));

    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/c",
            Some(RawRef::Direct(oid(2))),
            Expected::Any,
        ))
        .expect("txn");
    let prepared = store.prepare(txn).expect("prepare");

    let store2 = Arc::clone(&store);
    let err = thread::spawn(move || {
        let txn2 = RefTransaction::new()
            .update(update(
                "refs/heads/d",
                Some(RawRef::Direct(oid(3))),
                Expected::Missing,
            ))
            .expect("txn");
        match store2.prepare(txn2) {
            Err(err) => err,
            Ok(_) => panic!("expected lock"),
        }
    })
    .join()
    .expect("thread");
    assert!(matches!(err, RefStoreError::LockHeld { .. }));

    drop(prepared);
}

fn reflog_roundtrip(factory: &StoreFactory) {
    let store = factory();
    store.create_reflog("refs/heads/r").expect("create");
    let entries = vec![
        ReflogEntry {
            old_oid: oid(1),
            new_oid: oid(2),
            identity: "A <a@a.com> 1 +0000".to_owned(),
            message: "m1".to_owned(),
        },
        ReflogEntry {
            old_oid: oid(2),
            new_oid: oid(3),
            identity: "A <a@a.com> 2 +0000".to_owned(),
            message: "m2".to_owned(),
        },
    ];
    store
        .replace_reflog("refs/heads/r", entries.clone())
        .expect("replace");

    let mut forward = Vec::new();
    store
        .for_each_reflog_entry("refs/heads/r", false, &mut |e| {
            forward.push(e.clone());
            ControlFlow::Continue(())
        })
        .expect("forward");
    assert_eq!(forward, entries);

    let mut reverse = Vec::new();
    store
        .for_each_reflog_entry("refs/heads/r", true, &mut |e| {
            reverse.push(e.clone());
            ControlFlow::Continue(())
        })
        .expect("reverse");
    assert_eq!(reverse, entries.iter().rev().cloned().collect::<Vec<_>>());

    assert!(store.reflog_exists("refs/heads/r").expect("exists"));
    store.delete_reflog("refs/heads/r").expect("delete");
    assert!(!store.reflog_exists("refs/heads/r").expect("exists"));

    let mut reflog_refs = Vec::new();
    store
        .replace_reflog(
            "refs/heads/rr",
            vec![ReflogEntry {
                old_oid: oid(1),
                new_oid: oid(2),
                identity: "x".to_owned(),
                message: String::new(),
            }],
        )
        .expect("seed reflog ref");
    store
        .for_each_reflog_ref(&mut |name| {
            reflog_refs.push(name.to_owned());
            ControlFlow::Continue(())
        })
        .expect("list");
    assert_eq!(reflog_refs, vec!["refs/heads/rr"]);
}
