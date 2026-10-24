//! Building blocks for custom [`RefStore`](super::RefStore) backends.
//!
//! A backend that keeps refs somewhere other than a git directory (a database, a network
//! service, an in-process map) does not have to re-derive Git's ref-update rules. The helpers
//! here are the ones the built-in backends use:
//!
//! - [`check_transaction`] runs every check [`RefStore::prepare`](super::RefStore::prepare)
//!   must make: ref names ([`validate_storable_refname`]), directory/file conflicts
//!   ([`verify_create_conflicts`]), compare-and-swap expectations ([`expected_matches`],
//!   through symbolic refs) and two updates writing the same stored ref.
//! - [`apply_transaction`] computes the refs and reflogs after a commit, including
//!   symbolic-ref dereferencing and the reflog lines appended to both the symbolic ref and
//!   its target.
//! - [`resolve_symbolic`] follows symbolic refs inside a [`RefBatchState`].
//!
//! A typical store loads the refs (and reflogs) a transaction touches into a
//! [`RefBatchState`], calls [`check_transaction`] in `prepare` while holding its lock, and
//! at commit applies [`apply_transaction`] to a copy and persists the difference.
//! `MemoryRefStore` is implemented exactly this way. Run the
//! `grit_test_support::refstore_conformance` suite to check a backend against the built-ins.

use std::collections::BTreeMap;

use crate::objects::ObjectId;
use crate::refs::SYMREF_MAXDEPTH;

use super::apply::{
    apply_case_only_ref_rename, apply_reflog_rename, apply_update, simulate_batch_apply,
};
use super::error::RefStoreError;
use super::transaction::RefTransaction;
use super::validation::validate_storable_refname as validate_name;
use super::{RawRef, RefUpdate};

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

/// Validate every ref name a transaction writes, including reflog and case-only renames.
///
/// # Errors
///
/// Returns [`RefStoreError::InvalidRefName`] for the first name Git would refuse to store.
pub fn validate_transaction_names(txn: &RefTransaction) -> Result<(), RefStoreError> {
    for update in txn.updates() {
        validate_name(&update.name)?;
    }
    for rename in txn.reflog_renames() {
        validate_name(&rename.from)?;
        validate_name(&rename.to)?;
    }
    for rename in txn.case_only_renames() {
        validate_name(&rename.from)?;
        validate_name(&rename.to)?;
    }
    Ok(())
}

/// Run every check `prepare` must make for `txn` against the current `state`.
///
/// `state.refs` must contain every stored ref the transaction names, the targets of any
/// symbolic refs among them, and every stored ref that is an ancestor or descendant (in the
/// `a` / `a/b` directory sense) of a ref the transaction creates. Loading the whole store is
/// always correct. `state.reflogs` may be empty; reflogs are not consulted by the checks.
///
/// # Errors
///
/// - [`RefStoreError::DuplicateUpdate`] when two updates name, or dereference to, the same
///   stored ref.
/// - [`RefStoreError::InvalidRefName`] for a name Git refuses to store.
/// - [`RefStoreError::NameUnavailable`] for a directory/file conflict with a stored ref or
///   another ref in the batch.
/// - [`RefStoreError::ExpectedMismatch`] when an [`Expected`](super::Expected) check fails.
/// - [`RefStoreError::SymrefLoop`] when dereferencing exceeds the symbolic-ref depth limit.
pub fn check_transaction(state: &RefBatchState, txn: &RefTransaction) -> Result<(), RefStoreError> {
    RefTransaction::validate_no_duplicates(txn.updates())?;
    validate_transaction_names(txn)?;
    verify_create_conflicts(&state.refs, txn.updates())?;
    simulate_batch_apply(
        state,
        txn.updates(),
        txn.reflog_renames(),
        txn.case_only_renames(),
    )
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
