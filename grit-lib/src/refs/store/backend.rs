//! Building blocks for custom [`RefStore`](super::RefStore) backends.
//!
//! A backend that keeps refs somewhere other than a git directory (a database, a network
//! service, an in-process map) does not have to re-derive Git's ref-update rules. The helpers
//! here are the ones the built-in backends use:
//!
//! - [`check_transaction`] runs every check [`RefStore::prepare`](super::RefStore::prepare)
//!   must make: ref names and null ids ([`validate_transaction`]), directory/file conflicts
//!   ([`verify_create_conflicts`]), compare-and-swap expectations ([`expected_matches`],
//!   through symbolic refs) and two updates writing the same stored ref.
//! - [`apply_transaction`] computes the refs and reflogs after a commit, including
//!   symbolic-ref dereferencing and the reflog lines appended to both the symbolic ref and
//!   its target.
//! - [`resolve_symbolic`] follows symbolic refs inside a [`RefBatchState`].
//!
//! A typical store loads the refs (and reflogs) a transaction touches into a
//! [`RefBatchState`], calls [`check_transaction`] in `prepare` while holding its lock, and
//! at commit applies [`apply_transaction`] in place on that map when it owns the state (or
//! on a copy while iterators still share it) and persists the difference.
//! `MemoryRefStore` is implemented exactly this way. Run the
//! `grit_test_support::refstore_conformance` suite to check a backend against the built-ins.

use std::collections::BTreeMap;

use crate::objects::{ObjectId, ObjectKind};
use crate::refs::SYMREF_MAXDEPTH;

use super::apply::{
    apply_case_only_ref_rename, apply_reflog_rename, apply_update, simulate_batch_apply,
};
use super::error::{BackendError, RefStoreError};
use super::transaction::RefTransaction;
use super::validation::validate_storable_refname as validate_name;
use super::{RawRef, RefUpdate};

type PartitionUpdatesResult = (Vec<RefUpdate>, Vec<(String, RefStoreError)>);

pub use super::apply::RefBatchState;
pub use super::transaction::expected_matches;
pub use super::validation::verify_create_conflicts;

/// Reject `name` when Git refuses to store it (path traversal, illegal components, a
/// `.lock` suffix, an empty `refs/` name, …).
///
/// `HEAD` and other root refs such as `FETCH_HEAD` are accepted.
///
/// # Errors
///
/// Returns [`RefStoreError::InvalidRefName`] naming the rejected ref.
pub fn validate_storable_refname(name: &str) -> Result<(), RefStoreError> {
    validate_name(name)
}

/// The checks every backend makes before taking locks: each update name and both sides of
/// every reflog and case-only rename must be safe to store, and an update other than a
/// reflog-only one may not store the null object id.
///
/// [`check_transaction`] runs this first; call it on its own when your store must reject a
/// malformed batch before acquiring its lock.
///
/// # Errors
///
/// Returns [`RefStoreError::InvalidRefName`] for the first name Git would refuse to store,
/// [`RefStoreError::InvalidSymrefTarget`] for a symbolic value Git would refuse,
/// [`RefStoreError::NullOid`] for an update pointing a ref at the null id, or
/// [`RefStoreError::InvalidReflog`] for forged reflog metadata.
pub fn validate_transaction(txn: &RefTransaction) -> Result<(), RefStoreError> {
    txn.validate()
}

/// Run every check `prepare` must make for `txn` against the current `state`.
///
/// `state.refs` must contain every stored ref the transaction names, the targets of any
/// symbolic refs among them, and every stored ref that is an ancestor or descendant (in the
/// `a` / `a/b` directory sense) of a ref the transaction creates. Loading the whole store is
/// always correct; [`check_transaction`] still costs only `O` of the transaction's names,
/// symref depth, and directory probes—not the size of `state.refs`. `state.reflogs` may be
/// empty; reflogs are not consulted by the checks.
///
/// # Errors
///
/// - [`RefStoreError::DuplicateUpdate`] when two updates name, or dereference to, the same
///   stored ref.
/// - [`RefStoreError::InvalidRefName`] for a name Git refuses to store,
///   [`RefStoreError::InvalidSymrefTarget`] for a symbolic target Git refuses, and
///   [`RefStoreError::NullOid`] for an update storing the null id.
/// - [`RefStoreError::NameUnavailable`] for a directory/file conflict with a stored ref or
///   another ref in the batch.
/// - [`RefStoreError::ExpectedMismatch`] when an [`Expected`](super::Expected) check fails.
/// - [`RefStoreError::SymrefLoop`] when dereferencing exceeds the symbolic-ref depth limit.
pub fn check_transaction(state: &RefBatchState, txn: &RefTransaction) -> Result<(), RefStoreError> {
    txn.validate()?;
    verify_create_conflicts(&state.refs, txn.updates())?;
    simulate_batch_apply(
        state,
        txn.updates(),
        txn.reflog_renames(),
        txn.case_only_renames(),
    )
}

/// Check that each direct value in `updates` names an object, and that a branch or `HEAD`
/// names a commit.
///
/// `kind_of` reports the stored kind of an object, or `None` when it is absent. Deletes,
/// symbolic values and the null id (rejected as [`RefStoreError::NullOid`] by
/// [`validate_transaction`]) are not looked up. Reflog-only updates and explicit reflog
/// object ids on other updates are checked like direct values. Updates flagged
/// [`RefUpdateFlags::skip_oid_verification`](super::RefUpdateFlags::skip_oid_verification)
/// are not looked up. The branch rule applies to the name the update gives, before any
/// symbolic dereference.
///
/// # Errors
///
/// [`RefStoreError::MissingObject`], [`RefStoreError::NotACommit`], or
/// [`RefStoreError::ObjectLookup`] wrapping a `kind_of` failure, for the first offending
/// update.
pub(crate) fn verify_direct_targets(
    updates: &[RefUpdate],
    mut kind_of: impl FnMut(&ObjectId) -> crate::error::Result<Option<ObjectKind>>,
) -> Result<(), RefStoreError> {
    for update in updates {
        if update.flags.skip_oid_verification {
            continue;
        }
        for oid in oids_to_verify_for_update(update) {
            verify_stored_oid_for_ref(&update.name, oid, &mut kind_of)?;
        }
    }
    Ok(())
}

fn oids_to_verify_for_update(update: &RefUpdate) -> Vec<ObjectId> {
    if update.new_value.is_none() && !update.flags.log_only {
        return Vec::new();
    }
    let mut oids = Vec::new();
    if update.flags.log_only {
        if let Some(oid) = update.reflog.as_ref().and_then(|log| log.new_oid) {
            push_nonzero_oid(&mut oids, oid);
        } else if let Some(RawRef::Direct(oid)) = &update.new_value {
            push_nonzero_oid(&mut oids, *oid);
        }
    } else if let Some(RawRef::Direct(oid)) = &update.new_value {
        push_nonzero_oid(&mut oids, *oid);
    }
    if let Some(oid) = update.reflog.as_ref().and_then(|log| log.new_oid) {
        push_nonzero_oid(&mut oids, oid);
    }
    if let Some(oid) = update.reflog.as_ref().and_then(|log| log.old_oid) {
        push_nonzero_oid(&mut oids, oid);
    }
    oids
}

fn push_nonzero_oid(out: &mut Vec<ObjectId>, oid: ObjectId) {
    if !oid.is_zero() && !out.contains(&oid) {
        out.push(oid);
    }
}

fn verify_stored_oid_for_ref(
    name: &str,
    oid: ObjectId,
    kind_of: &mut impl FnMut(&ObjectId) -> crate::error::Result<Option<ObjectKind>>,
) -> Result<(), RefStoreError> {
    let kind = kind_of(&oid).map_err(|err| RefStoreError::ObjectLookup {
        name: name.to_owned(),
        oid,
        source: BackendError::new(err),
    })?;
    match kind {
        None => Err(RefStoreError::MissingObject {
            name: name.to_owned(),
            oid,
        }),
        Some(kind) if kind != ObjectKind::Commit && names_branch(name) => {
            Err(RefStoreError::NotACommit {
                name: name.to_owned(),
                oid,
                kind,
            })
        }
        Some(_) => Ok(()),
    }
}

/// Whether Git requires `name` to point at a commit.
fn names_branch(name: &str) -> bool {
    name == "HEAD" || name.starts_with("refs/heads/")
}

/// Directory/file, compare-and-swap and duplicate-write checks for `updates` alone (names
/// already validated, no renames).
pub(crate) fn check_transaction_updates(
    state: &RefBatchState,
    updates: &[RefUpdate],
) -> Result<(), RefStoreError> {
    verify_create_conflicts(&state.refs, updates)?;
    simulate_batch_apply(state, updates, &[], &[])
}

/// Whether a ref-store error may reject a single update while siblings still commit.
#[must_use]
pub fn is_per_ref_rejectable(err: &RefStoreError) -> bool {
    matches!(
        err,
        RefStoreError::ExpectedMismatch { .. }
            | RefStoreError::NameUnavailable { .. }
            | RefStoreError::MissingObject { .. }
            | RefStoreError::NotACommit { .. }
    )
}

/// Split `updates` into those that pass batch checks in order and per-ref rejections.
///
/// Simulates accepted updates on `state` so later creates see earlier ones in the same batch.
/// Non-rejectable errors fail the whole call.
pub fn partition_updates_allowing_failures(
    state: &RefBatchState,
    updates: Vec<RefUpdate>,
) -> Result<PartitionUpdatesResult, RefStoreError> {
    let mut working = state.clone();
    let mut accepted = Vec::with_capacity(updates.len());
    let mut rejected = Vec::new();
    for update in updates {
        let name = update.name.clone();
        match check_transaction_updates(&working, std::slice::from_ref(&update)) {
            Ok(()) => {
                apply_update(&mut working, &update)?;
                accepted.push(update);
            }
            Err(err) if is_per_ref_rejectable(&err) => rejected.push((name, err)),
            Err(err) => return Err(err),
        }
    }
    Ok((accepted, rejected))
}

/// Apply `txn` to `state`: ref values, symbolic-ref dereferencing, appended reflog lines,
/// case-only renames and reflog renames.
///
/// Updates apply in transaction order. A direct-value update through a symbolic ref (without
/// [`RefUpdateFlags::no_deref`](super::RefUpdateFlags::no_deref)) writes the end of the
/// symbolic chain and logs the same entry on the symbolic ref and on that target. A delete
/// with a [`ReflogUpdate`](super::ReflogUpdate) appends an entry whose new id is zero and
/// keeps the reflog. Call [`check_transaction`] first; this function does not re-check
/// expectations, so on error `state` may be partially updated (apply to a copy).
///
/// # Errors
///
/// Returns [`RefStoreError::SymrefLoop`] for a too-deep symbolic chain, or
/// [`RefStoreError::ExpectedMismatch`] / [`RefStoreError::Corrupt`] when a case-only rename
/// finds its source missing or its destination taken.
pub fn apply_transaction(
    state: &mut RefBatchState,
    txn: &RefTransaction,
) -> Result<(), RefStoreError> {
    for update in txn.updates() {
        apply_update(state, update)?;
    }
    for rename in txn.case_only_renames() {
        apply_case_only_ref_rename(state, rename)?;
    }
    for rename in txn.reflog_renames() {
        apply_reflog_rename(state, rename)?;
    }
    Ok(())
}

/// Resolve `name` to an object id through symbolic refs stored in `refs`.
///
/// # Errors
///
/// Returns [`RefStoreError::NotFound`] naming the missing ref (the requested name or the
/// dangling target), or [`RefStoreError::SymrefLoop`] after
/// [`SYMREF_MAXDEPTH`] hops.
pub fn resolve_symbolic(
    refs: &BTreeMap<String, RawRef>,
    name: &str,
) -> Result<ObjectId, RefStoreError> {
    let mut current = name;
    for _ in 0..SYMREF_MAXDEPTH {
        match refs.get(current) {
            None => return Err(RefStoreError::not_found(current)),
            Some(RawRef::Direct(oid)) => return Ok(*oid),
            Some(RawRef::Symbolic(target)) => current = target,
        }
    }
    Err(RefStoreError::SymrefLoop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::store::{Expected, RefUpdateFlags, ReflogUpdate};
    use crate::refs::RefnameUnavailable;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("valid oid")
    }

    fn put(name: &str, value: RawRef, expected: Expected) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: Some(value),
            expected,
            reflog: None,
            flags: RefUpdateFlags::default(),
        }
    }

    fn state(refs: &[(&str, RawRef)]) -> RefBatchState {
        RefBatchState {
            refs: refs
                .iter()
                .map(|(name, value)| ((*name).to_owned(), value.clone()))
                .collect(),
            reflogs: BTreeMap::new(),
        }
    }

    #[test]
    fn check_rejects_bad_names_before_anything_else() {
        let txn = RefTransaction::new()
            .update(put(
                "refs/heads/a..b",
                RawRef::Direct(oid(1)),
                Expected::Missing,
            ))
            .expect("txn");
        let err = check_transaction(&RefBatchState::default(), &txn).expect_err("bad name");
        assert!(
            matches!(&err, RefStoreError::InvalidRefName { name } if name == "refs/heads/a..b")
        );
        assert!(validate_storable_refname("HEAD").is_ok());
        assert!(validate_storable_refname("refs/heads/ok").is_ok());
    }

    #[test]
    fn check_transaction_cost_is_flat_in_store_size() {
        use std::time::Instant;

        fn oid_n(n: u32) -> ObjectId {
            let mut b = [0u8; 20];
            b[..4].copy_from_slice(&(n + 1).to_be_bytes());
            ObjectId::from_bytes(&b).expect("valid oid")
        }

        let mut large = RefBatchState::default();
        for i in 0..20_000u32 {
            large
                .refs
                .insert(format!("refs/heads/b{i:05}"), RawRef::Direct(oid_n(i)));
        }
        let small = RefBatchState::default();
        let txn = |i: u32| {
            RefTransaction::new()
                .update(RefUpdate {
                    name: format!("refs/heads/new{i}"),
                    new_value: Some(RawRef::Direct(oid_n(i))),
                    expected: Expected::Missing,
                    reflog: None,
                    flags: RefUpdateFlags::default(),
                })
                .expect("txn")
        };
        let samples = 30u32;
        let start = Instant::now();
        for i in 0..samples {
            check_transaction(&large, &txn(i)).expect("large");
        }
        let large_elapsed = start.elapsed();
        let start = Instant::now();
        for i in 0..samples {
            check_transaction(&small, &txn(i + samples)).expect("small");
        }
        let small_elapsed = start.elapsed();
        assert!(
            large_elapsed.as_secs() < 1,
            "large map checks took {:?}",
            large_elapsed
        );
        assert!(
            large_elapsed
                <= small_elapsed * 8 + small_elapsed / 2 + std::time::Duration::from_millis(1),
            "large {:?} vs small {:?}",
            large_elapsed,
            small_elapsed
        );
    }

    #[test]
    fn check_reports_df_conflicts_both_ways() {
        let txn = RefTransaction::new()
            .update(put(
                "refs/heads/a",
                RawRef::Direct(oid(2)),
                Expected::Missing,
            ))
            .expect("txn");
        let stored = state(&[("refs/heads/a/b", RawRef::Direct(oid(1)))]);
        let err = check_transaction(&stored, &txn).expect_err("descendant blocks");
        assert!(matches!(
            err,
            RefStoreError::NameUnavailable {
                reason: RefnameUnavailable::DescendantExists { .. }
            }
        ));

        let txn = RefTransaction::new()
            .update(put(
                "refs/heads/a/b/c",
                RawRef::Direct(oid(2)),
                Expected::Missing,
            ))
            .expect("txn");
        let err = check_transaction(&stored, &txn).expect_err("ancestor blocks");
        assert!(matches!(
            err,
            RefStoreError::NameUnavailable {
                reason: RefnameUnavailable::AncestorExists { .. }
            }
        ));
    }

    #[test]
    fn check_compares_through_symrefs() {
        let stored = state(&[
            ("refs/heads/main", RawRef::Direct(oid(1))),
            ("HEAD", RawRef::Symbolic("refs/heads/main".to_owned())),
        ]);
        let good = RefTransaction::new()
            .update(put("HEAD", RawRef::Direct(oid(2)), Expected::Oid(oid(1))))
            .expect("txn");
        check_transaction(&stored, &good).expect("peeled old value matches");
        let stale = RefTransaction::new()
            .update(put("HEAD", RawRef::Direct(oid(2)), Expected::Oid(oid(9))))
            .expect("txn");
        assert!(matches!(
            check_transaction(&stored, &stale),
            Err(RefStoreError::ExpectedMismatch { .. })
        ));
    }

    #[test]
    fn apply_logs_symref_and_target() {
        let mut stored = state(&[
            ("refs/heads/main", RawRef::Direct(oid(1))),
            ("HEAD", RawRef::Symbolic("refs/heads/main".to_owned())),
        ]);
        let mut update = put("HEAD", RawRef::Direct(oid(2)), Expected::Oid(oid(1)));
        update.reflog = Some(ReflogUpdate {
            identity: "T <t@example.com>".to_owned(),
            message: "commit: two".to_owned(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: None,
            new_oid: None,
        });
        let txn = RefTransaction::new().update(update).expect("txn");
        check_transaction(&stored, &txn).expect("check");
        apply_transaction(&mut stored, &txn).expect("apply");

        assert_eq!(
            stored.refs.get("HEAD"),
            Some(&RawRef::Symbolic("refs/heads/main".to_owned()))
        );
        assert_eq!(
            resolve_symbolic(&stored.refs, "HEAD").expect("resolve"),
            oid(2)
        );
        for name in ["HEAD", "refs/heads/main"] {
            let log = stored.reflogs.get(name).expect("reflog");
            assert_eq!(log.len(), 1, "{name}");
            assert_eq!((log[0].old_oid, log[0].new_oid), (oid(1), oid(2)), "{name}");
            assert_eq!(log[0].identity, "T <t@example.com> 0 +0000", "{name}");
        }
    }

    #[test]
    fn verify_direct_targets_checks_presence_and_branch_kind() {
        let kinds = |oid: &ObjectId| {
            Ok(match oid.as_bytes()[19] {
                1 => Some(ObjectKind::Commit),
                2 => Some(ObjectKind::Tree),
                3 => Some(ObjectKind::Tag),
                _ => None,
            })
        };
        let at = |name: &str, byte: u8| put(name, RawRef::Direct(oid(byte)), Expected::Any);

        for name in ["HEAD", "refs/heads/a", "refs/tags/a", "refs/remotes/o/a"] {
            assert_eq!(
                verify_direct_targets(&[at(name, 1)], kinds),
                Ok(()),
                "{name}"
            );
            assert_eq!(
                verify_direct_targets(&[at(name, 9)], kinds),
                Err(RefStoreError::MissingObject {
                    name: name.to_owned(),
                    oid: oid(9)
                }),
                "{name}"
            );
        }
        for name in ["HEAD", "refs/heads/a"] {
            assert_eq!(
                verify_direct_targets(&[at(name, 2)], kinds),
                Err(RefStoreError::NotACommit {
                    name: name.to_owned(),
                    oid: oid(2),
                    kind: ObjectKind::Tree
                })
            );
        }
        for name in [
            "refs/tags/a",
            "refs/remotes/o/a",
            "refs/notes/commits",
            "ORIG_HEAD",
        ] {
            assert_eq!(
                verify_direct_targets(&[at(name, 2)], kinds),
                Ok(()),
                "{name}"
            );
            assert_eq!(
                verify_direct_targets(&[at(name, 3)], kinds),
                Ok(()),
                "{name}"
            );
        }
    }

    #[test]
    fn verify_direct_targets_skips_values_it_cannot_or_must_not_check() {
        let mut probes = 0;
        let mut log_only = put("refs/heads/a", RawRef::Direct(oid(9)), Expected::Any);
        log_only.flags.log_only = true;
        let mut skipped = put("refs/heads/b", RawRef::Direct(oid(9)), Expected::Any);
        skipped.flags.skip_oid_verification = true;
        let delete = RefUpdate {
            new_value: None,
            ..put("refs/heads/c", RawRef::Direct(oid(9)), Expected::Any)
        };
        let updates = [
            log_only,
            skipped,
            delete,
            put(
                "HEAD",
                RawRef::Symbolic("refs/heads/x".to_owned()),
                Expected::Any,
            ),
            put(
                "refs/heads/z",
                RawRef::Direct(ObjectId::zero()),
                Expected::Any,
            ),
        ];
        let result = verify_direct_targets(&updates, |_| {
            probes += 1;
            Ok(None)
        });
        assert_eq!(
            result,
            Err(RefStoreError::MissingObject {
                name: "refs/heads/a".to_owned(),
                oid: oid(9)
            })
        );
        assert_eq!(probes, 1);
    }

    #[test]
    fn verify_direct_targets_checks_log_only_explicit_reflog_oids_without_new_value() {
        let log_only = RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: None,
            expected: Expected::Any,
            reflog: Some(super::super::ReflogUpdate {
                identity: "A <a@example.com>".to_owned(),
                message: "log".to_owned(),
                time: time::OffsetDateTime::UNIX_EPOCH,
                old_oid: Some(oid(1)),
                new_oid: Some(oid(9)),
            }),
            flags: RefUpdateFlags {
                log_only: true,
                ..Default::default()
            },
        };
        assert_eq!(
            verify_direct_targets(&[log_only], |_| Ok(None)),
            Err(RefStoreError::MissingObject {
                name: "refs/heads/main".to_owned(),
                oid: oid(9)
            })
        );
        let ok = RefUpdate {
            name: "refs/heads/main".to_owned(),
            new_value: None,
            expected: Expected::Any,
            reflog: Some(super::super::ReflogUpdate {
                identity: "A <a@example.com>".to_owned(),
                message: "log".to_owned(),
                time: time::OffsetDateTime::UNIX_EPOCH,
                old_oid: Some(oid(1)),
                new_oid: Some(oid(1)),
            }),
            flags: RefUpdateFlags {
                log_only: true,
                ..Default::default()
            },
        };
        assert_eq!(
            verify_direct_targets(&[ok], |_| Ok(Some(ObjectKind::Commit))),
            Ok(())
        );
    }

    #[test]
    fn verify_direct_targets_reports_lookup_failures_and_stops_at_the_first_bad_update() {
        let updates = [
            put("refs/heads/a", RawRef::Direct(oid(1)), Expected::Any),
            put("refs/heads/b", RawRef::Direct(oid(2)), Expected::Any),
            put("refs/heads/c", RawRef::Direct(oid(3)), Expected::Any),
        ];
        let mut seen = Vec::new();
        let err = verify_direct_targets(&updates, |id| {
            seen.push(*id);
            match id.as_bytes()[19] {
                1 => Ok(Some(ObjectKind::Commit)),
                _ => Err(crate::error::Error::CorruptObject("bad header".to_owned())),
            }
        })
        .expect_err("lookup failure");
        let RefStoreError::ObjectLookup {
            name,
            oid: id,
            source,
        } = err
        else {
            panic!("expected ObjectLookup, got {err:?}");
        };
        assert_eq!((name.as_str(), id), ("refs/heads/b", oid(2)));
        assert!(matches!(
            source.downcast_ref::<crate::error::Error>(),
            Some(crate::error::Error::CorruptObject(_))
        ));
        assert_eq!(seen, vec![oid(1), oid(2)]);
    }

    #[test]
    fn resolve_symbolic_reports_missing_target_and_loops() {
        let refs = state(&[
            ("HEAD", RawRef::Symbolic("refs/heads/unborn".to_owned())),
            ("refs/heads/x", RawRef::Symbolic("refs/heads/y".to_owned())),
            ("refs/heads/y", RawRef::Symbolic("refs/heads/x".to_owned())),
        ])
        .refs;
        assert!(matches!(
            resolve_symbolic(&refs, "HEAD"),
            Err(RefStoreError::NotFound { name }) if name == "refs/heads/unborn"
        ));
        assert!(matches!(
            resolve_symbolic(&refs, "refs/heads/x"),
            Err(RefStoreError::SymrefLoop)
        ));
    }
}
