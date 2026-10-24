//! Reusable conformance checks for [`RefStore`] backends.
//!
//! Every built-in backend (files, reftable, memory) runs this suite, so a custom store that
//! passes it agrees with them on compare-and-swap, directory/file conflicts, name validation,
//! symbolic-ref dereferencing, reflogs and transaction locking. Wire a factory that returns a
//! fresh, **empty** store (no refs, no reflogs, no `HEAD`) into [`run_suite`]:
//!
//! ```no_run
//! use std::sync::Arc;
//!
//! use grit_lib::refs::store::{MemoryRefStore, RefStore};
//! use grit_test_support::refstore_conformance::run_suite;
//!
//! run_suite(|| Arc::new(MemoryRefStore::new()) as Arc<dyn RefStore>);
//! ```
//!
//! Each case is also exposed as its own function taking a fresh empty store, so test harnesses
//! can name failures precisely. The helpers in [`grit_lib::refs::store::backend`] implement the
//! semantics these cases check.

#![allow(clippy::expect_used, clippy::panic)]

use std::ops::ControlFlow;
use std::sync::Arc;
use std::thread;

use grit_lib::error::Error;
use grit_lib::objects::ObjectId;
use grit_lib::refs::reflog::ReflogEntry;
use grit_lib::refs::store::{
    Expected, RawRef, RefStore, RefStoreError, RefTransaction, RefUpdate, RefUpdateFlags,
    ReflogUpdate,
};

/// One conformance case: receives a fresh empty store and panics on a violation.
pub type Case = fn(Arc<dyn RefStore>);

/// Every case in the suite, with its name, in the order [`run_suite`] runs them.
pub const CASES: &[(&str, Case)] = &[
    ("cas_success", cas_success),
    ("cas_failure", cas_failure),
    ("create_only_missing", create_only_missing),
    ("delete_missing_ref", delete_missing_ref),
    (
        "resolve_missing_ref_is_not_found",
        resolve_missing_ref_is_not_found,
    ),
    ("invalid_ref_names_rejected", invalid_ref_names_rejected),
    ("df_conflict_in_batch", df_conflict_in_batch),
    (
        "df_conflict_against_stored_ancestor",
        df_conflict_against_stored_ancestor,
    ),
    (
        "df_conflict_against_stored_descendant",
        df_conflict_against_stored_descendant,
    ),
    (
        "df_conflict_not_freed_by_delete_in_batch",
        df_conflict_not_freed_by_delete_in_batch,
    ),
    (
        "symref_deref_state_and_reflog",
        symref_deref_state_and_reflog,
    ),
    (
        "symref_deref_reflog_on_symref_and_target",
        symref_deref_reflog_on_symref_and_target,
    ),
    (
        "symref_no_deref_state_and_reflog",
        symref_no_deref_state_and_reflog,
    ),
    ("symbolic_iteration_peel", symbolic_iteration_peel),
    ("direct_update_reflog_oids", direct_update_reflog_oids),
    ("delete_with_reflog_oids", delete_with_reflog_oids),
    ("symref_deref_delete", symref_deref_delete),
    (
        "symref_head_deref_delete_recreate_branch",
        symref_head_deref_delete_recreate_branch,
    ),
    ("symref_no_deref_delete", symref_no_deref_delete),
    (
        "symref_unborn_referent_created",
        symref_unborn_referent_created,
    ),
    (
        "symref_dual_deref_conflicting_target_rejected",
        symref_dual_deref_conflicting_target_rejected,
    ),
    (
        "batch_apply_rejected_leaves_store_unchanged",
        batch_apply_rejected_leaves_store_unchanged,
    ),
    ("empty_reflog_exists", empty_reflog_exists),
    ("prefix_iteration_order", prefix_iteration_order),
    ("abort_leaves_state_unchanged", abort_leaves_state_unchanged),
    ("abort_releases_lock", abort_releases_lock),
    (
        "dropped_prepared_transaction_releases_lock",
        dropped_prepared_transaction_releases_lock,
    ),
    ("concurrent_prepare_lock_held", concurrent_prepare_lock_held),
    ("reflog_roundtrip", reflog_roundtrip),
];

/// Run every case in [`CASES`], calling `factory` once per case for a fresh empty store.
///
/// # Panics
///
/// Panics with the case name when the store violates a case.
pub fn run_suite(factory: impl Fn() -> Arc<dyn RefStore>) {
    for (name, case) in CASES {
        let store = factory();
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| case(store)));
        if let Err(payload) = outcome {
            let detail = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            panic!("ref store conformance case `{name}` failed: {detail}");
        }
    }
}

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
        old_oid: None,
        new_oid: None,
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

fn commit_txn(store: &dyn RefStore, txn: RefTransaction) {
    store
        .prepare(txn)
        .expect("prepare")
        .commit()
        .expect("commit");
}

fn seed_direct(store: &dyn RefStore, name: &str, value: ObjectId) {
    let txn = RefTransaction::new()
        .update(update(name, Some(RawRef::Direct(value)), Expected::Missing))
        .expect("txn");
    commit_txn(store, txn);
}

fn seed_symref(store: &dyn RefStore, name: &str, target: &str) {
    let txn = RefTransaction::new()
        .update(update(
            name,
            Some(RawRef::Symbolic(target.to_owned())),
            Expected::Missing,
        ))
        .expect("txn");
    commit_txn(store, txn);
}

fn prepare_err(store: &dyn RefStore, txn: RefTransaction) -> RefStoreError {
    match store.prepare(txn) {
        Err(err) => err,
        Ok(_) => panic!("expected prepare to fail"),
    }
}

fn newest_reflog_entry(store: &dyn RefStore, name: &str) -> Option<ReflogEntry> {
    let mut last = None;
    store
        .for_each_reflog_entry(name, true, &mut |e| {
            last = Some(e.clone());
            ControlFlow::Break(())
        })
        .expect("reflog");
    last
}

/// A compare-and-swap update with the right old value commits.
pub fn cas_success(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/main", oid(1));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/main",
            Some(RawRef::Direct(oid(2))),
            Expected::Oid(oid(1)),
        ))
        .expect("txn");
    commit_txn(store.as_ref(), txn);
    assert_eq!(
        store.read_raw("refs/heads/main").expect("read"),
        Some(RawRef::Direct(oid(2)))
    );
}

/// A compare-and-swap update with a stale old value fails in `prepare` and changes nothing.
pub fn cas_failure(store: Arc<dyn RefStore>) {
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

/// [`Expected::Missing`] creates a ref once and rejects the second create.
pub fn create_only_missing(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/new", oid(3));
    assert_eq!(
        store.read_raw("refs/heads/new").expect("read"),
        Some(RawRef::Direct(oid(3)))
    );

    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/new",
            Some(RawRef::Direct(oid(4))),
            Expected::Missing,
        ))
        .expect("txn");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::ExpectedMismatch { .. }
    ));
}

/// Deleting an absent ref with [`Expected::Exists`] fails.
pub fn delete_missing_ref(store: Arc<dyn RefStore>) {
    let txn = RefTransaction::new()
        .update(update("refs/heads/gone", None, Expected::Exists))
        .expect("txn");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::ExpectedMismatch { .. }
    ));
}

/// Resolving a missing ref, or a symref whose target is missing, names the missing ref.
pub fn resolve_missing_ref_is_not_found(store: Arc<dyn RefStore>) {
    let missing = store.resolve("refs/heads/absent").expect_err("missing ref");
    assert!(
        matches!(
            missing,
            Error::RefStore(RefStoreError::NotFound { ref name }) if name == "refs/heads/absent"
        ),
        "{missing:?}"
    );

    seed_symref(store.as_ref(), "refs/heads/alias", "refs/heads/unborn");
    let dangling = store
        .resolve("refs/heads/alias")
        .expect_err("dangling symref");
    assert!(
        matches!(
            dangling,
            Error::RefStore(RefStoreError::NotFound { ref name }) if name == "refs/heads/unborn"
        ),
        "{dangling:?}"
    );
}

/// Names Git refuses to store fail `prepare` with [`RefStoreError::InvalidRefName`].
pub fn invalid_ref_names_rejected(store: Arc<dyn RefStore>) {
    for name in [
        "refs/heads/../escape",
        "refs/heads/has space",
        "refs/heads/topic.lock",
        "refs/heads//double",
        "refs/heads/trailing/",
        "refs/",
    ] {
        let txn = RefTransaction::new()
            .update(update(name, Some(RawRef::Direct(oid(1))), Expected::Any))
            .expect("txn");
        let err = prepare_err(store.as_ref(), txn);
        assert!(
            matches!(&err, RefStoreError::InvalidRefName { name: got } if got == name),
            "{name:?}: {err:?}"
        );
        assert_eq!(store.read_raw(name).ok().flatten(), None, "{name:?}");
    }
}

/// Two refs in one batch where one is a directory prefix of the other conflict.
pub fn df_conflict_in_batch(store: Arc<dyn RefStore>) {
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
    assert_eq!(store.read_raw("refs/heads/p").expect("read"), None);
}

/// Creating `a/b` while `a` is stored conflicts.
pub fn df_conflict_against_stored_ancestor(store: Arc<dyn RefStore>) {
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
    assert_eq!(
        store.read_raw("refs/heads/parent/leaf").expect("read"),
        None
    );
}

/// Creating `a` while `a/b` is stored conflicts.
pub fn df_conflict_against_stored_descendant(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/a/b", oid(1));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/a",
            Some(RawRef::Direct(oid(2))),
            Expected::Missing,
        ))
        .expect("txn");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::NameUnavailable { .. }
    ));
    assert_eq!(store.read_raw("refs/heads/a").expect("read"), None);
    assert_eq!(
        store.read_raw("refs/heads/a/b").expect("read"),
        Some(RawRef::Direct(oid(1)))
    );
}

/// Deleting `a/b` and creating `a` in the same batch still conflicts (as in Git).
pub fn df_conflict_not_freed_by_delete_in_batch(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/a/b", oid(1));
    let txn = RefTransaction::new()
        .update(update("refs/heads/a/b", None, Expected::Oid(oid(1))))
        .expect("delete")
        .update(update(
            "refs/heads/a",
            Some(RawRef::Direct(oid(2))),
            Expected::Missing,
        ))
        .expect("create");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::NameUnavailable { .. }
    ));
    assert_eq!(
        store.read_raw("refs/heads/a/b").expect("read"),
        Some(RawRef::Direct(oid(1)))
    );
}

/// A direct update through a symref moves the target and logs old/new on the symref.
pub fn symref_deref_state_and_reflog(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/target", oid(10));
    seed_symref(store.as_ref(), "refs/heads/sym", "refs/heads/target");

    let mut upd = update(
        "refs/heads/sym",
        Some(RawRef::Direct(oid(12))),
        Expected::Any,
    );
    upd.reflog = Some(sample_log());
    commit_txn(
        store.as_ref(),
        RefTransaction::new().update(upd).expect("txn"),
    );

    assert_eq!(
        store.read_raw("refs/heads/sym").expect("sym"),
        Some(RawRef::Symbolic("refs/heads/target".to_owned()))
    );
    assert_eq!(
        store.read_raw("refs/heads/target").expect("target"),
        Some(RawRef::Direct(oid(12)))
    );

    let entry = newest_reflog_entry(store.as_ref(), "refs/heads/sym").expect("entry");
    assert_eq!(entry.old_oid, oid(10));
    assert_eq!(entry.new_oid, oid(12));
}

/// Updating `HEAD` through its symref appends the same entry to `HEAD` and the branch log.
pub fn symref_deref_reflog_on_symref_and_target(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/main", oid(1));
    seed_symref(store.as_ref(), "HEAD", "refs/heads/main");

    let mut upd = update("HEAD", Some(RawRef::Direct(oid(2))), Expected::Oid(oid(1)));
    upd.reflog = Some(sample_log());
    commit_txn(
        store.as_ref(),
        RefTransaction::new().update(upd).expect("txn"),
    );

    assert_eq!(
        store.read_raw("refs/heads/main").expect("main"),
        Some(RawRef::Direct(oid(2)))
    );
    for name in ["HEAD", "refs/heads/main"] {
        let entry = newest_reflog_entry(store.as_ref(), name)
            .unwrap_or_else(|| panic!("{name} has no reflog entry"));
        assert_eq!(entry.old_oid, oid(1), "{name}");
        assert_eq!(entry.new_oid, oid(2), "{name}");
        assert_eq!(entry.message, "commit: msg", "{name}");
    }
}

/// With `no_deref`, a direct update replaces the symref itself and logs a zero old id.
pub fn symref_no_deref_state_and_reflog(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/t2", oid(11));
    seed_symref(store.as_ref(), "refs/heads/s2", "refs/heads/t2");

    let mut upd = update(
        "refs/heads/s2",
        Some(RawRef::Direct(oid(12))),
        Expected::Any,
    );
    upd.reflog = Some(sample_log());
    upd.flags.no_deref = true;
    commit_txn(
        store.as_ref(),
        RefTransaction::new().update(upd).expect("txn"),
    );

    assert_eq!(
        store.read_raw("refs/heads/s2").expect("s2"),
        Some(RawRef::Direct(oid(12)))
    );
    assert_eq!(
        store.read_raw("refs/heads/t2").expect("t2"),
        Some(RawRef::Direct(oid(11)))
    );

    let entry = newest_reflog_entry(store.as_ref(), "refs/heads/s2").expect("entry");
    assert!(entry.old_oid.is_zero());
    assert_eq!(entry.new_oid, oid(12));
}

/// Iteration reports the peeled id of a symbolic ref.
pub fn symbolic_iteration_peel(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/main", oid(1));
    seed_symref(store.as_ref(), "HEAD", "refs/heads/main");

    let mut peeled = None;
    store
        .for_each_ref("HEAD", &mut |entry| {
            peeled = entry.peeled;
            ControlFlow::Break(())
        })
        .expect("iter");
    assert_eq!(peeled, Some(oid(1)));
}

/// A direct update logs the previous and new ids.
pub fn direct_update_reflog_oids(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/r", oid(1));

    let mut upd = update("refs/heads/r", Some(RawRef::Direct(oid(2))), Expected::Any);
    upd.reflog = Some(sample_log());
    commit_txn(
        store.as_ref(),
        RefTransaction::new().update(upd).expect("txn"),
    );

    let entry = newest_reflog_entry(store.as_ref(), "refs/heads/r").expect("entry");
    assert_eq!(entry.old_oid, oid(1));
    assert_eq!(entry.new_oid, oid(2));
}

/// A delete with a reflog line logs the old id and a zero new id.
pub fn delete_with_reflog_oids(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/del", oid(5));

    let mut upd = update("refs/heads/del", None, Expected::Any);
    upd.reflog = Some(sample_log());
    commit_txn(
        store.as_ref(),
        RefTransaction::new().update(upd).expect("txn"),
    );

    assert_eq!(store.read_raw("refs/heads/del").expect("read"), None);
    let entry = newest_reflog_entry(store.as_ref(), "refs/heads/del").expect("entry");
    assert_eq!(entry.old_oid, oid(5));
    assert!(entry.new_oid.is_zero());
}

/// Deleting through a symref removes the target and keeps the symref.
pub fn symref_deref_delete(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/target", oid(1));
    seed_symref(store.as_ref(), "refs/heads/sym", "refs/heads/target");

    let txn = RefTransaction::new()
        .update(update("refs/heads/sym", None, Expected::Any))
        .expect("txn");
    commit_txn(store.as_ref(), txn);

    assert_eq!(
        store.read_raw("refs/heads/sym").expect("sym"),
        Some(RawRef::Symbolic("refs/heads/target".to_owned()))
    );
    assert_eq!(store.read_raw("refs/heads/target").expect("target"), None);
}

/// Deleting `HEAD`'s branch through `HEAD` and recreating it in the same batch is allowed.
pub fn symref_head_deref_delete_recreate_branch(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/main", oid(1));
    seed_symref(store.as_ref(), "HEAD", "refs/heads/main");

    let txn = RefTransaction::new()
        .update(update("HEAD", None, Expected::Any))
        .expect("delete head deref")
        .update(update(
            "refs/heads/main",
            Some(RawRef::Direct(oid(2))),
            Expected::Missing,
        ))
        .expect("recreate main");
    commit_txn(store.as_ref(), txn);

    assert_eq!(
        store.read_raw("HEAD").expect("head"),
        Some(RawRef::Symbolic("refs/heads/main".to_owned()))
    );
    assert_eq!(
        store.read_raw("refs/heads/main").expect("main"),
        Some(RawRef::Direct(oid(2)))
    );
}

/// With `no_deref`, deleting a symref removes only the symref.
pub fn symref_no_deref_delete(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/target", oid(1));
    seed_symref(store.as_ref(), "refs/heads/sym", "refs/heads/target");

    let mut upd = update("refs/heads/sym", None, Expected::Any);
    upd.flags.no_deref = true;
    commit_txn(
        store.as_ref(),
        RefTransaction::new().update(upd).expect("txn"),
    );

    assert_eq!(store.read_raw("refs/heads/sym").expect("sym"), None);
    assert_eq!(
        store.read_raw("refs/heads/target").expect("target"),
        Some(RawRef::Direct(oid(1)))
    );
}

/// Updating `HEAD` on an unborn branch creates the branch.
pub fn symref_unborn_referent_created(store: Arc<dyn RefStore>) {
    seed_symref(store.as_ref(), "HEAD", "refs/heads/unborn");

    let txn = RefTransaction::new()
        .update(update("HEAD", Some(RawRef::Direct(oid(7))), Expected::Any))
        .expect("txn");
    commit_txn(store.as_ref(), txn);

    assert_eq!(
        store.read_raw("HEAD").expect("head"),
        Some(RawRef::Symbolic("refs/heads/unborn".to_owned()))
    );
    assert_eq!(
        store.read_raw("refs/heads/unborn").expect("unborn"),
        Some(RawRef::Direct(oid(7)))
    );

    let txn2 = RefTransaction::new()
        .update(update(
            "refs/heads/first",
            Some(RawRef::Direct(oid(1))),
            Expected::Missing,
        ))
        .expect("first")
        .update(update("HEAD", Some(RawRef::Direct(oid(8))), Expected::Any))
        .expect("head");
    commit_txn(store.as_ref(), txn2);
    assert_eq!(
        store.read_raw("refs/heads/first").expect("first"),
        Some(RawRef::Direct(oid(1)))
    );
    assert_eq!(
        store.read_raw("refs/heads/unborn").expect("unborn"),
        Some(RawRef::Direct(oid(8)))
    );
}

/// Two symrefs dereferencing to the same target in one batch are a duplicate update.
pub fn symref_dual_deref_conflicting_target_rejected(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/target", oid(1));
    seed_symref(store.as_ref(), "refs/heads/sym1", "refs/heads/target");
    seed_symref(store.as_ref(), "refs/heads/sym2", "refs/heads/target");

    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/sym1",
            Some(RawRef::Direct(oid(2))),
            Expected::Any,
        ))
        .expect("sym1")
        .update(update(
            "refs/heads/sym2",
            Some(RawRef::Direct(oid(3))),
            Expected::Any,
        ))
        .expect("sym2");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::DuplicateUpdate {
            name
        } if name == "refs/heads/target"
    ));
    assert_eq!(
        store.read_raw("refs/heads/target").expect("target"),
        Some(RawRef::Direct(oid(1)))
    );
}

/// A batch whose later update fails its check leaves every ref unchanged.
pub fn batch_apply_rejected_leaves_store_unchanged(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/target", oid(1));
    seed_symref(store.as_ref(), "refs/heads/sym", "refs/heads/target");

    let txn = RefTransaction::new()
        .update(update("refs/heads/sym", None, Expected::Any))
        .expect("delete")
        .update(update(
            "refs/heads/target",
            Some(RawRef::Direct(oid(2))),
            Expected::Exists,
        ))
        .expect("update");
    assert!(matches!(
        prepare_err(store.as_ref(), txn),
        RefStoreError::ExpectedMismatch { .. }
    ));
    assert_eq!(
        store.read_raw("refs/heads/target").expect("target"),
        Some(RawRef::Direct(oid(1)))
    );
    assert_eq!(
        store.read_raw("refs/heads/sym").expect("sym"),
        Some(RawRef::Symbolic("refs/heads/target".to_owned()))
    );
}

/// An empty reflog (created or replaced with no entries) still exists.
pub fn empty_reflog_exists(store: Arc<dyn RefStore>) {
    store.create_reflog("refs/heads/empty").expect("create");
    assert!(store.reflog_exists("refs/heads/empty").expect("exists"));

    store
        .replace_reflog("refs/heads/empty2", Vec::new())
        .expect("replace empty");
    assert!(store.reflog_exists("refs/heads/empty2").expect("exists2"));

    let mut count = 0usize;
    store
        .for_each_reflog_entry("refs/heads/empty2", false, &mut |_| {
            count += 1;
            ControlFlow::Continue(())
        })
        .expect("iter empty");
    assert_eq!(count, 0);
}

/// Prefix iteration is sorted and treats the prefix as a string prefix.
pub fn prefix_iteration_order(store: Arc<dyn RefStore>) {
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

/// Aborting a prepared transaction changes nothing.
pub fn abort_leaves_state_unchanged(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/stay", oid(5));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/stay",
            Some(RawRef::Direct(oid(6))),
            Expected::Any,
        ))
        .expect("txn");
    store.prepare(txn).expect("prepare").abort().expect("abort");
    assert_eq!(
        store.read_raw("refs/heads/stay").expect("read"),
        Some(RawRef::Direct(oid(5)))
    );
}

/// After `abort`, the next `prepare` succeeds.
pub fn abort_releases_lock(store: Arc<dyn RefStore>) {
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/one",
            Some(RawRef::Direct(oid(1))),
            Expected::Missing,
        ))
        .expect("txn");
    store.prepare(txn).expect("prepare").abort().expect("abort");
    seed_direct(store.as_ref(), "refs/heads/one", oid(2));
    assert_eq!(
        store.read_raw("refs/heads/one").expect("read"),
        Some(RawRef::Direct(oid(2)))
    );
}

/// Dropping a prepared transaction without `commit` or `abort` releases its locks and
/// changes nothing.
pub fn dropped_prepared_transaction_releases_lock(store: Arc<dyn RefStore>) {
    seed_direct(store.as_ref(), "refs/heads/kept", oid(1));
    let txn = RefTransaction::new()
        .update(update(
            "refs/heads/kept",
            Some(RawRef::Direct(oid(2))),
            Expected::Oid(oid(1)),
        ))
        .expect("txn");
    drop(store.prepare(txn).expect("prepare"));
    assert_eq!(
        store.read_raw("refs/heads/kept").expect("read"),
        Some(RawRef::Direct(oid(1)))
    );

    let retry = RefTransaction::new()
        .update(update(
            "refs/heads/kept",
            Some(RawRef::Direct(oid(3))),
            Expected::Oid(oid(1)),
        ))
        .expect("retry");
    match store.prepare(retry) {
        Ok(prepared) => prepared.commit().expect("commit after drop"),
        Err(err) => panic!("prepare after dropping a prepared transaction failed: {err:?}"),
    }
    assert_eq!(
        store.read_raw("refs/heads/kept").expect("read"),
        Some(RawRef::Direct(oid(3)))
    );
}

/// While one transaction is prepared, `prepare` on another thread fails with
/// [`RefStoreError::LockHeld`].
pub fn concurrent_prepare_lock_held(store: Arc<dyn RefStore>) {
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
                "refs/heads/c",
                Some(RawRef::Direct(oid(3))),
                Expected::Any,
            ))
            .expect("txn");
        match store2.prepare(txn2) {
            Err(err) => err,
            Ok(_) => panic!("expected lock"),
        }
    })
    .join()
    .expect("thread");
    assert!(matches!(err, RefStoreError::LockHeld { .. }), "{err:?}");

    prepared.commit().expect("commit");
    assert_eq!(
        store.read_raw("refs/heads/c").expect("read"),
        Some(RawRef::Direct(oid(2)))
    );
}

/// Reflogs replace, iterate in both directions, delete and list.
pub fn reflog_roundtrip(store: Arc<dyn RefStore>) {
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
