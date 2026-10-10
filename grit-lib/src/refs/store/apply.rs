//! Shared ref-update semantics for in-memory simulation and commit application.

use std::collections::{BTreeMap, BTreeSet};

use crate::diff::zero_oid;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

use super::error::RefStoreError;
use super::semantics::format_reflog_identity;
use super::transaction::expected_matches;
use super::types::{RawRef, RefUpdate, RefUpdateFlags, ReflogUpdate};

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

/// Verify CAS and apply every update to `trial`, returning the first error.
pub fn simulate_batch_apply(state: &RefBatchState, updates: &[RefUpdate]) -> StoreResult<()> {
    let mut trial = state.snapshot();
    for update in updates {
        let actual = trial.refs.get(&update.name);
        if !expected_matches(actual, &update.expected) {
            return Err(RefStoreError::ExpectedMismatch {
                name: update.name.clone(),
                expected: update.expected.clone(),
                actual: actual.cloned(),
            });
        }
        apply_update(&mut trial, update)?;
    }
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

    apply_ref_change(state, update)?;

    if let (Some(log), Some((old_oid, new_oid))) = (&update.reflog, reflog_oids) {
        append_reflog_entry(state, &update.name, old_oid, new_oid, log);
    }
    Ok(())
}

fn apply_ref_change(state: &mut RefBatchState, update: &RefUpdate) -> StoreResult<()> {
    if update.flags.log_only {
        return Ok(());
    }
    match &update.new_value {
        None => {
            if should_deref_symref(state, update) {
                let target = symref_peel_write_target(&state.refs, &update.name)?;
                state.refs.remove(&target);
            } else {
                state.refs.remove(&update.name);
            }
        }
        Some(new_value) => {
            if should_deref_symref_update(state, update) {
                let RawRef::Direct(oid) = new_value else {
                    return Err(RefStoreError::Corrupt(
                        "deref update requires direct oid".to_owned(),
                    ));
                };
                let target = symref_peel_write_target(&state.refs, &update.name)?;
                state.refs.insert(target, RawRef::Direct(*oid));
            } else {
                state.refs.insert(update.name.clone(), new_value.clone());
            }
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
    resolved_oid_at_name(state, &update.name, update.flags).unwrap_or_else(|_| zero_oid())
}

fn reflog_new_oid_before(state: &RefBatchState, update: &RefUpdate) -> ObjectId {
    if update.flags.log_only {
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
        return Err(RefStoreError::Corrupt(format!("ref not found: {name}")));
    };
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) => resolve_map(refs, target, depth + 1),
    }
}
