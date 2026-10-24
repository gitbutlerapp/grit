//! Shared ref-update semantics for in-memory simulation and commit application.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::diff::zero_oid;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

use super::error::RefStoreError;
use super::semantics::format_reflog_identity;
use super::transaction::expected_matches;
use super::types::{
    Expected, RawRef, RefCaseOnlyRename, RefReflogRename, RefUpdate, RefUpdateFlags, ReflogUpdate,
};

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// Mutable ref + reflog state used to simulate or apply a transaction batch.
#[derive(Debug, Clone, Default)]
pub struct RefBatchState {
    /// Stored ref values keyed by full ref name.
    pub refs: BTreeMap<String, RawRef>,
    /// Reflog entries keyed by ref name (oldest first).
    pub reflogs: BTreeMap<String, Vec<ReflogEntry>>,
}

impl RefBatchState {
    /// Clone refs and reflogs without any prepared-transaction metadata.
    #[must_use]
    pub fn snapshot(&self) -> Self {
        Self {
            refs: self.refs.clone(),
            reflogs: self.reflogs.clone(),
        }
    }
}

/// Whether `update.expected` is satisfied given current batch state (including symref peel for CAS).
pub fn expected_matches_for_update(state: &RefBatchState, update: &RefUpdate) -> bool {
    if let Expected::Oid(want) = &update.expected {
        if should_deref_symref_update(state, update) {
            return reflog_old_oid_before(state, update) == *want;
        }
    }
    let actual = state.refs.get(&update.name);
    expected_matches(actual, &update.expected)
}

/// One physical ref file affected by an update (directly or via symref peel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageWriteKind {
    Deleted,
    Written(ObjectId),
}

/// Reject batches that would touch the same storage ref twice, except delete-then-create.
pub(crate) fn check_storage_write(
    prior: &mut HashMap<String, StorageWriteKind>,
    state: &RefBatchState,
    update: &RefUpdate,
) -> StoreResult<()> {
    let Some((name, kind)) = storage_write_for_update(state, update)? else {
        return Ok(());
    };
    match prior.get(&name) {
        None => {
            prior.insert(name, kind);
            Ok(())
        }
        Some(StorageWriteKind::Deleted) if matches!(kind, StorageWriteKind::Written(_)) => {
            prior.insert(name, kind);
            Ok(())
        }
        Some(_) => Err(RefStoreError::DuplicateUpdate { name }),
    }
}

fn storage_write_for_update(
    state: &RefBatchState,
    update: &RefUpdate,
) -> StoreResult<Option<(String, StorageWriteKind)>> {
    if update.flags.log_only {
        return Ok(None);
    }
    match &update.new_value {
        None => {
            if should_deref_symref(state, update) {
                let target = symref_peel_write_target(&state.refs, &update.name)?;
                Ok(Some((target, StorageWriteKind::Deleted)))
            } else {
                Ok(Some((update.name.clone(), StorageWriteKind::Deleted)))
            }
        }
        Some(RawRef::Direct(oid)) if should_deref_symref_update(state, update) => {
            let target = symref_peel_write_target(&state.refs, &update.name)?;
            Ok(Some((target, StorageWriteKind::Written(*oid))))
        }
        Some(RawRef::Direct(oid)) => {
            Ok(Some((update.name.clone(), StorageWriteKind::Written(*oid))))
        }
        Some(RawRef::Symbolic(_)) => Ok(None),
    }
}

/// Verify CAS and apply every update to `trial`, returning the first error.
pub fn simulate_batch_apply(
    state: &RefBatchState,
    updates: &[RefUpdate],
    reflog_renames: &[RefReflogRename],
    case_only_renames: &[RefCaseOnlyRename],
) -> StoreResult<()> {
    let mut trial = state.snapshot();
    let mut storage_writes = HashMap::new();
    for update in updates {
        if !expected_matches_for_update(&trial, update) {
            return Err(RefStoreError::ExpectedMismatch {
                name: update.name.clone(),
                expected: update.expected.clone(),
                actual: trial.refs.get(&update.name).cloned(),
            });
        }
        check_storage_write(&mut storage_writes, &trial, update)?;
        apply_update(&mut trial, update)?;
    }
    for rename in case_only_renames {
        apply_case_only_ref_rename(&mut trial, rename)?;
    }
    for rename in reflog_renames {
        apply_reflog_rename(&mut trial, rename)?;
    }
    Ok(())
}

/// Move a ref entry when only the spelling's case changes.
pub fn apply_case_only_ref_rename(
    state: &mut RefBatchState,
    rename: &RefCaseOnlyRename,
) -> StoreResult<()> {
    if rename.from == rename.to {
        return Ok(());
    }
    let value = state
        .refs
        .remove(&rename.from)
        .ok_or_else(|| RefStoreError::ExpectedMismatch {
            name: rename.from.clone(),
            expected: Expected::Exists,
            actual: None,
        })?;
    if state.refs.contains_key(&rename.to) {
        return Err(RefStoreError::Corrupt(format!(
            "case-only rename blocked: '{}' already exists",
            rename.to
        )));
    }
    state.refs.insert(rename.to.clone(), value);
    Ok(())
}

/// Move `rename.from`'s reflog to `rename.to` and append `rename.append`.
pub fn apply_reflog_rename(state: &mut RefBatchState, rename: &RefReflogRename) -> StoreResult<()> {
    let mut entries = state.reflogs.remove(&rename.from).unwrap_or_default();
    state.reflogs.remove(&rename.to);
    let old_oid = rename
        .append
        .old_oid
        .unwrap_or_else(|| entries.last().map(|e| e.new_oid).unwrap_or_else(zero_oid));
    let new_oid = rename.append.new_oid.unwrap_or(old_oid);
    entries.push(ReflogEntry {
        old_oid,
        new_oid,
        identity: format_reflog_identity(&rename.append.identity, rename.append.time),
        message: rename.append.message.clone(),
    });
    state.reflogs.insert(rename.to.clone(), entries);
    Ok(())
}

/// Apply one update (ref change + optional reflog) to `state`.
pub fn apply_update(state: &mut RefBatchState, update: &RefUpdate) -> StoreResult<()> {
    let reflog_oids = update.reflog.as_ref().map(|_| {
        (
            reflog_old_oid_before(state, update),
            reflog_new_oid_before(state, update),
        )
    });
    let deref_target_reflog = update.reflog.as_ref().and_then(|_| {
        if !should_deref_symref_update(state, update) {
            return None;
        }
        symref_peel_write_target(&state.refs, &update.name).ok()
    });

    apply_ref_change(state, update)?;

    if let (Some(log), Some((old_oid, new_oid))) = (&update.reflog, reflog_oids) {
        append_reflog_entry(state, &update.name, old_oid, new_oid, log);
        if let Some(target) = deref_target_reflog {
            append_reflog_entry(state, &target, old_oid, new_oid, log);
        }
    }
    Ok(())
}

/// The one stored ref [`apply_update`] changes in `state`: `update.name`, or the end of its
/// symref chain when the update dereferences. `None` for reflog-only updates.
///
/// After the update that ref holds `update.new_value`; every other ref keeps its value.
///
/// # Errors
///
/// [`RefStoreError::SymrefLoop`] when the symref chain is too deep.
pub(crate) fn ref_change_target(
    state: &RefBatchState,
    update: &RefUpdate,
) -> StoreResult<Option<String>> {
    if update.flags.log_only {
        return Ok(None);
    }
    let deref = match &update.new_value {
        None => should_deref_symref(state, update),
        Some(_) => should_deref_symref_update(state, update),
    };
    if deref {
        symref_peel_write_target(&state.refs, &update.name).map(Some)
    } else {
        Ok(Some(update.name.clone()))
    }
}

fn apply_ref_change(state: &mut RefBatchState, update: &RefUpdate) -> StoreResult<()> {
    let Some(target) = ref_change_target(state, update)? else {
        return Ok(());
    };
    match &update.new_value {
        None => {
            state.refs.remove(&target);
        }
        Some(new_value) => {
            state.refs.insert(target, new_value.clone());
        }
    }
    Ok(())
}

fn should_deref_symref(state: &RefBatchState, update: &RefUpdate) -> bool {
    !update.flags.no_deref && matches!(state.refs.get(&update.name), Some(RawRef::Symbolic(_)))
}

fn should_deref_symref_update(state: &RefBatchState, update: &RefUpdate) -> bool {
    should_deref_symref(state, update)
        && matches!(update.new_value.as_ref(), Some(RawRef::Direct(_)))
}

fn symref_peel_write_target(
    refs: &BTreeMap<String, RawRef>,
    sym_name: &str,
) -> StoreResult<String> {
    let Some(RawRef::Symbolic(first)) = refs.get(sym_name) else {
        return Err(RefStoreError::Corrupt(format!(
            "not a symbolic ref: {sym_name}"
        )));
    };
    resolve_symref_peel_target(refs, first)
}

fn resolve_symref_peel_target(refs: &BTreeMap<String, RawRef>, start: &str) -> StoreResult<String> {
    let mut name = start.to_owned();
    let mut depth = 0;
    loop {
        if depth >= SYMREF_MAXDEPTH {
            return Err(RefStoreError::SymrefLoop);
        }
        match refs.get(name.as_str()) {
            Some(RawRef::Direct(_)) => return Ok(name),
            Some(RawRef::Symbolic(next)) => {
                name = next.clone();
                depth += 1;
            }
            None => return Ok(name),
        }
    }
}

fn append_reflog_entry(
    state: &mut RefBatchState,
    name: &str,
    old_oid: ObjectId,
    new_oid: ObjectId,
    log: &ReflogUpdate,
) {
    let identity = format_reflog_identity(&log.identity, log.time);
    let entry = ReflogEntry {
        old_oid,
        new_oid,
        identity,
        message: log.message.clone(),
    };
    state
        .reflogs
        .entry(name.to_owned())
        .or_default()
        .push(entry);
}

/// Ref names that must be locked for a batch (update targets plus symref peel targets).
pub(crate) fn ref_lock_names_for_updates(
    state: &RefBatchState,
    updates: &[RefUpdate],
) -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for update in updates {
        if update.flags.log_only {
            continue;
        }
        names.insert(update.name.clone());
        if should_deref_symref(state, update) {
            if let Ok(target) = symref_peel_write_target(&state.refs, &update.name) {
                names.insert(target);
            }
        }
        if should_deref_symref_update(state, update) {
            if let Ok(target) = symref_peel_write_target(&state.refs, &update.name) {
                names.insert(target);
            }
        }
    }
    names
}

fn reflog_old_oid_before(state: &RefBatchState, update: &RefUpdate) -> ObjectId {
    if let Some(oid) = update.reflog.as_ref().and_then(|log| log.old_oid) {
        return oid;
    }
    resolved_oid_at_name(state, &update.name, update.flags).unwrap_or_else(|_| zero_oid())
}

fn reflog_new_oid_before(state: &RefBatchState, update: &RefUpdate) -> ObjectId {
    if let Some(oid) = update.reflog.as_ref().and_then(|log| log.new_oid) {
        return oid;
    }
    if update.flags.log_only {
        if let Some(RawRef::Direct(oid)) = &update.new_value {
            return *oid;
        }
        return reflog_old_oid_before(state, update);
    }
    match &update.new_value {
        None => zero_oid(),
        Some(RawRef::Direct(oid)) if should_deref_symref_update(state, update) => *oid,
        Some(value) => {
            oid_for_reflog_value(state, value, update.flags).unwrap_or_else(|_| zero_oid())
        }
    }
}

fn resolved_oid_at_name(
    state: &RefBatchState,
    name: &str,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    let Some(value) = state.refs.get(name) else {
        return Ok(zero_oid());
    };
    oid_for_reflog_value(state, value, flags)
}

fn oid_for_reflog_value(
    state: &RefBatchState,
    value: &RawRef,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(_target) if flags.no_deref => Ok(zero_oid()),
        RawRef::Symbolic(target) => resolve_map(&state.refs, target, 0),
    }
}

/// Resolve `name` through symbolic refs in `refs`.
pub fn resolve_map(
    refs: &BTreeMap<String, RawRef>,
    name: &str,
    depth: usize,
) -> StoreResult<ObjectId> {
    if depth >= SYMREF_MAXDEPTH {
        return Err(RefStoreError::SymrefLoop);
    }
    let Some(value) = refs.get(name) else {
        return Err(RefStoreError::not_found(name));
    };
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) => resolve_map(refs, target, depth + 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::refs::store::Expected;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("valid oid")
    }

    fn direct_update(name: &str, value: ObjectId) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Direct(value)),
            expected: Expected::Any,
            reflog: None,
            flags: RefUpdateFlags::default(),
        }
    }

    #[test]
    fn ref_change_target_names_the_one_ref_an_update_writes() {
        let mut state = RefBatchState::default();
        state
            .refs
            .insert("refs/heads/main".to_owned(), RawRef::Direct(oid(1)));
        state.refs.insert(
            "HEAD".to_owned(),
            RawRef::Symbolic("refs/heads/main".to_owned()),
        );
        let target = |update: &RefUpdate| ref_change_target(&state, update).expect("target");

        let direct = direct_update("HEAD", oid(2));
        assert_eq!(target(&direct).as_deref(), Some("refs/heads/main"));
        let delete = RefUpdate {
            new_value: None,
            ..direct.clone()
        };
        assert_eq!(target(&delete).as_deref(), Some("refs/heads/main"));
        let no_deref = RefUpdate {
            flags: RefUpdateFlags {
                no_deref: true,
                ..Default::default()
            },
            ..direct.clone()
        };
        assert_eq!(target(&no_deref).as_deref(), Some("HEAD"));
        let symbolic = RefUpdate {
            new_value: Some(RawRef::Symbolic("refs/heads/other".to_owned())),
            ..direct.clone()
        };
        assert_eq!(target(&symbolic).as_deref(), Some("HEAD"));
        let log_only = RefUpdate {
            flags: RefUpdateFlags {
                log_only: true,
                ..Default::default()
            },
            ..direct
        };
        assert_eq!(target(&log_only), None);

        for update in [delete, no_deref, symbolic] {
            let mut after = state.clone();
            apply_update(&mut after, &update).expect("apply");
            let changed: Vec<&String> = state
                .refs
                .keys()
                .chain(after.refs.keys())
                .filter(|name| state.refs.get(*name) != after.refs.get(*name))
                .collect();
            let written = target(&update).expect("writes a ref");
            assert!(changed.iter().all(|name| **name == written), "{changed:?}");
            assert_eq!(after.refs.get(&written), update.new_value.as_ref());
        }
    }

    #[test]
    fn dual_symref_deref_to_same_target_is_rejected() {
        let mut state = RefBatchState::default();
        state
            .refs
            .insert("refs/heads/target".to_owned(), RawRef::Direct(oid(1)));
        state.refs.insert(
            "refs/heads/sym1".to_owned(),
            RawRef::Symbolic("refs/heads/target".to_owned()),
        );
        state.refs.insert(
            "refs/heads/sym2".to_owned(),
            RawRef::Symbolic("refs/heads/target".to_owned()),
        );
        let updates = vec![
            direct_update("refs/heads/sym1", oid(2)),
            direct_update("refs/heads/sym2", oid(3)),
        ];
        let err =
            simulate_batch_apply(&state, &updates, &[], &[]).expect_err("conflicting peel writes");
        assert!(matches!(
            err,
            RefStoreError::DuplicateUpdate {
                name
            } if name == "refs/heads/target"
        ));
    }

    #[test]
    fn symref_deref_delete_then_recreate_target_is_allowed() {
        let mut state = RefBatchState::default();
        state
            .refs
            .insert("refs/heads/main".to_owned(), RawRef::Direct(oid(1)));
        state.refs.insert(
            "HEAD".to_owned(),
            RawRef::Symbolic("refs/heads/main".to_owned()),
        );
        let updates = vec![
            RefUpdate {
                name: "HEAD".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            },
            RefUpdate {
                name: "refs/heads/main".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Missing,
                reflog: None,
                flags: RefUpdateFlags::default(),
            },
        ];
        simulate_batch_apply(&state, &updates, &[], &[])
            .expect("delete-then-create on peeled target");
    }

    #[test]
    fn log_only_direct_new_value_sets_reflog_new_oid() {
        let mut state = RefBatchState::default();
        state
            .refs
            .insert("refs/heads/main".to_owned(), RawRef::Direct(oid(1)));
        state.refs.insert(
            "HEAD".to_owned(),
            RawRef::Symbolic("refs/heads/main".to_owned()),
        );
        let updates = vec![
            RefUpdate {
                name: "HEAD".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Any,
                reflog: Some(ReflogUpdate {
                    identity: "T <t@t>".into(),
                    message: "mirror".into(),
                    time: time::OffsetDateTime::UNIX_EPOCH,
                    old_oid: None,
                    new_oid: None,
                }),
                flags: RefUpdateFlags {
                    log_only: true,
                    ..Default::default()
                },
            },
            RefUpdate {
                name: "refs/heads/main".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Oid(oid(1)),
                reflog: None,
                flags: RefUpdateFlags::default(),
            },
        ];
        for update in &updates {
            apply_update(&mut state, update).expect("apply");
        }
        let head_log = state.reflogs.get("HEAD").expect("head log");
        assert_eq!(head_log[0].old_oid, oid(1));
        assert_eq!(head_log[0].new_oid, oid(2));
    }

    #[test]
    fn log_only_head_no_deref_skips_branch_reflog_mirror() {
        let mut state = RefBatchState::default();
        state
            .refs
            .insert("refs/heads/main".to_owned(), RawRef::Direct(oid(1)));
        state.refs.insert(
            "HEAD".to_owned(),
            RawRef::Symbolic("refs/heads/main".to_owned()),
        );
        let log = ReflogUpdate {
            identity: "T <t@t>".into(),
            message: "history rewrite".into(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: None,
            new_oid: None,
        };
        let updates = vec![
            RefUpdate {
                name: "HEAD".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Any,
                reflog: Some(ReflogUpdate {
                    old_oid: Some(oid(1)),
                    new_oid: Some(oid(2)),
                    ..log.clone()
                }),
                flags: RefUpdateFlags {
                    log_only: true,
                    no_deref: true,
                },
            },
            RefUpdate {
                name: "refs/heads/main".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Oid(oid(1)),
                reflog: Some(log),
                flags: RefUpdateFlags::default(),
            },
        ];
        for update in &updates {
            apply_update(&mut state, update).expect("apply");
        }
        let branch_log = state.reflogs.get("refs/heads/main").expect("branch log");
        assert_eq!(branch_log.len(), 1);
        let head_log = state.reflogs.get("HEAD").expect("head log");
        assert_eq!(head_log.len(), 1);
        assert_eq!(head_log[0].old_oid, oid(1));
        assert_eq!(head_log[0].new_oid, oid(2));
        assert_eq!(branch_log[0].old_oid, head_log[0].old_oid);
        assert_eq!(branch_log[0].new_oid, head_log[0].new_oid);
    }
}
