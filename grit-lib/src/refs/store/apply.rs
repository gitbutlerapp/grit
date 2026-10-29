//! Shared ref-update semantics for in-memory simulation and commit application.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::diff::zero_oid;
use crate::objects::ObjectId;
use crate::reflog::ReflogEntry;
use crate::refs::SYMREF_MAXDEPTH;

use super::error::RefStoreError;
use super::semantics::format_reflog_identity;
use super::transaction::expected_matches;
use super::transaction::RefTransaction;
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

/// Ref map for batch simulation: reads fall through to `base`, writes stay in `overlay`.
struct RefMapOverlay<'a> {
    base: &'a BTreeMap<String, RawRef>,
    overlay: HashMap<String, Option<RawRef>>,
}

impl<'a> RefMapOverlay<'a> {
    fn new(base: &'a BTreeMap<String, RawRef>) -> Self {
        Self {
            base,
            overlay: HashMap::new(),
        }
    }

    fn get(&self, name: &str) -> Option<Cow<'_, RawRef>> {
        match self.overlay.get(name) {
            Some(None) => None,
            Some(Some(value)) => Some(Cow::Borrowed(value)),
            None => self.base.get(name).map(Cow::Borrowed),
        }
    }

    fn insert(&mut self, name: String, value: RawRef) {
        self.overlay.insert(name, Some(value));
    }

    fn remove(&mut self, name: &str) {
        self.overlay.insert(name.to_owned(), None);
    }
}

enum RefsRead<'a, 'b> {
    Batch(&'a RefBatchState),
    Overlay(&'a RefMapOverlay<'b>),
}

impl RefsRead<'_, '_> {
    fn get(&self, name: &str) -> Option<Cow<'_, RawRef>> {
        match self {
            Self::Batch(state) => state.refs.get(name).map(Cow::Borrowed),
            Self::Overlay(overlay) => overlay.get(name),
        }
    }
}

enum RefsWrite<'a, 'b> {
    Overlay(&'a mut RefMapOverlay<'b>),
}

impl RefsWrite<'_, '_> {
    fn read(&self) -> RefsRead<'_, '_> {
        match self {
            Self::Overlay(overlay) => RefsRead::Overlay(overlay),
        }
    }

    fn insert(&mut self, name: String, value: RawRef) {
        match self {
            Self::Overlay(overlay) => overlay.insert(name, value),
        }
    }

    fn remove(&mut self, name: &str) {
        match self {
            Self::Overlay(overlay) => overlay.remove(name),
        }
    }

    fn contains_key(&self, name: &str) -> bool {
        self.read().get(name).is_some()
    }
}

struct RefWriteCtx<'a, 'b> {
    refs: RefsWrite<'a, 'b>,
    reflogs: &'a mut BTreeMap<String, Vec<ReflogEntry>>,
}

/// Whether `update.expected` is satisfied given current batch state (including symref peel for CAS).
pub fn expected_matches_for_update(state: &RefBatchState, update: &RefUpdate) -> bool {
    expected_matches_for_update_read(&RefsRead::Batch(state), update)
}

fn expected_matches_for_update_read(refs: &RefsRead<'_, '_>, update: &RefUpdate) -> bool {
    if let Expected::Oid(want) = &update.expected {
        if should_deref_symref_update_read(refs, update) {
            return reflog_old_oid_before_read(refs, update) == *want;
        }
    }
    let actual = refs.get(&update.name);
    expected_matches(actual.as_deref(), &update.expected)
}

/// One physical ref file affected by an update (directly or via symref peel).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StorageWriteKind {
    Deleted,
    Written(ObjectId),
}

fn check_storage_write_read(
    prior: &mut HashMap<String, StorageWriteKind>,
    refs: &RefsRead<'_, '_>,
    update: &RefUpdate,
) -> StoreResult<()> {
    let Some((name, kind)) = storage_write_for_update_read(refs, update)? else {
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

fn storage_write_for_update_read(
    refs: &RefsRead<'_, '_>,
    update: &RefUpdate,
) -> StoreResult<Option<(String, StorageWriteKind)>> {
    if update.flags.log_only {
        return Ok(None);
    }
    match &update.new_value {
        None => {
            if should_deref_symref_read(refs, update) {
                let target = symref_peel_write_target_read(refs, &update.name)?;
                Ok(Some((target, StorageWriteKind::Deleted)))
            } else {
                Ok(Some((update.name.clone(), StorageWriteKind::Deleted)))
            }
        }
        Some(RawRef::Direct(oid)) if should_deref_symref_update_read(refs, update) => {
            let target = symref_peel_write_target_read(refs, &update.name)?;
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
    let mut overlay = RefMapOverlay::new(&state.refs);
    let mut reflogs = BTreeMap::new();
    let mut storage_writes = HashMap::new();
    for update in updates {
        let read = RefsRead::Overlay(&overlay);
        if !expected_matches_for_update_read(&read, update) {
            return Err(RefStoreError::ExpectedMismatch {
                name: update.name.clone(),
                expected: update.expected.clone(),
                actual: read.get(&update.name).map(Cow::into_owned),
            });
        }
        check_storage_write_read(&mut storage_writes, &read, update)?;
        let mut ctx = RefWriteCtx {
            refs: RefsWrite::Overlay(&mut overlay),
            reflogs: &mut reflogs,
        };
        apply_update_ctx(&mut ctx, update)?;
    }
    for rename in case_only_renames {
        let mut ctx = RefWriteCtx {
            refs: RefsWrite::Overlay(&mut overlay),
            reflogs: &mut reflogs,
        };
        apply_case_only_ref_rename_ctx(&mut ctx, rename)?;
    }
    for rename in reflog_renames {
        apply_reflog_rename_map(&mut reflogs, rename)?;
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

fn apply_case_only_ref_rename_ctx(
    ctx: &mut RefWriteCtx<'_, '_>,
    rename: &RefCaseOnlyRename,
) -> StoreResult<()> {
    if rename.from == rename.to {
        return Ok(());
    }
    let value = match ctx.refs.read().get(&rename.from) {
        Some(v) => v.into_owned(),
        None => {
            return Err(RefStoreError::ExpectedMismatch {
                name: rename.from.clone(),
                expected: Expected::Exists,
                actual: None,
            });
        }
    };
    ctx.refs.remove(&rename.from);
    if ctx.refs.contains_key(&rename.to) {
        return Err(RefStoreError::Corrupt(format!(
            "case-only rename blocked: '{}' already exists",
            rename.to
        )));
    }
    ctx.refs.insert(rename.to.clone(), value);
    Ok(())
}

/// Move `rename.from`'s reflog to `rename.to` and append `rename.append`.
pub fn apply_reflog_rename(state: &mut RefBatchState, rename: &RefReflogRename) -> StoreResult<()> {
    apply_reflog_rename_map(&mut state.reflogs, rename)
}

fn apply_reflog_rename_map(
    reflogs: &mut BTreeMap<String, Vec<ReflogEntry>>,
    rename: &RefReflogRename,
) -> StoreResult<()> {
    let mut entries = reflogs.remove(&rename.from).unwrap_or_default();
    reflogs.remove(&rename.to);
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
    reflogs.insert(rename.to.clone(), entries);
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
        append_reflog_entry(&mut state.reflogs, &update.name, old_oid, new_oid, log);
        if let Some(target) = deref_target_reflog {
            append_reflog_entry(&mut state.reflogs, &target, old_oid, new_oid, log);
        }
    }
    Ok(())
}

fn apply_update_ctx(ctx: &mut RefWriteCtx<'_, '_>, update: &RefUpdate) -> StoreResult<()> {
    let read = ctx.refs.read();
    let reflog_oids = update.reflog.as_ref().map(|_| {
        (
            reflog_old_oid_before_read(&read, update),
            reflog_new_oid_before_read(&read, update),
        )
    });
    let deref_target_reflog = update.reflog.as_ref().and_then(|_| {
        if !should_deref_symref_update_read(&read, update) {
            return None;
        }
        symref_peel_write_target_read(&read, &update.name).ok()
    });

    apply_ref_change_ctx(ctx, update)?;

    if let (Some(log), Some((old_oid, new_oid))) = (&update.reflog, reflog_oids) {
        append_reflog_entry(ctx.reflogs, &update.name, old_oid, new_oid, log);
        if let Some(target) = deref_target_reflog {
            append_reflog_entry(ctx.reflogs, &target, old_oid, new_oid, log);
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

fn apply_ref_change_ctx(ctx: &mut RefWriteCtx<'_, '_>, update: &RefUpdate) -> StoreResult<()> {
    let Some(target) = ref_change_target_read(&ctx.refs.read(), update)? else {
        return Ok(());
    };
    match &update.new_value {
        None => ctx.refs.remove(&target),
        Some(new_value) => ctx.refs.insert(target, new_value.clone()),
    }
    Ok(())
}

fn ref_change_target_read(
    refs: &RefsRead<'_, '_>,
    update: &RefUpdate,
) -> StoreResult<Option<String>> {
    if update.flags.log_only {
        return Ok(None);
    }
    let deref = match &update.new_value {
        None => should_deref_symref_read(refs, update),
        Some(_) => should_deref_symref_update_read(refs, update),
    };
    if deref {
        symref_peel_write_target_read(refs, &update.name).map(Some)
    } else {
        Ok(Some(update.name.clone()))
    }
}

fn should_deref_symref(state: &RefBatchState, update: &RefUpdate) -> bool {
    should_deref_symref_read(&RefsRead::Batch(state), update)
}

fn should_deref_symref_read(refs: &RefsRead<'_, '_>, update: &RefUpdate) -> bool {
    !update.flags.no_deref && matches!(refs.get(&update.name).as_deref(), Some(RawRef::Symbolic(_)))
}

fn should_deref_symref_update(state: &RefBatchState, update: &RefUpdate) -> bool {
    should_deref_symref_update_read(&RefsRead::Batch(state), update)
}

fn should_deref_symref_update_read(refs: &RefsRead<'_, '_>, update: &RefUpdate) -> bool {
    should_deref_symref_read(refs, update)
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

fn symref_peel_write_target_read(refs: &RefsRead<'_, '_>, sym_name: &str) -> StoreResult<String> {
    let Some(value) = refs.get(sym_name) else {
        return Err(RefStoreError::Corrupt(format!(
            "not a symbolic ref: {sym_name}"
        )));
    };
    let RawRef::Symbolic(first) = value.as_ref() else {
        return Err(RefStoreError::Corrupt(format!(
            "not a symbolic ref: {sym_name}"
        )));
    };
    resolve_symref_peel_target_read(refs, first.as_str())
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
                name.clone_from(next);
                depth += 1;
            }
            None => return Ok(name),
        }
    }
}

fn resolve_symref_peel_target_read(refs: &RefsRead<'_, '_>, start: &str) -> StoreResult<String> {
    let mut name = start.to_owned();
    let mut depth = 0;
    loop {
        if depth >= SYMREF_MAXDEPTH {
            return Err(RefStoreError::SymrefLoop);
        }
        match refs.get(name.as_str()).as_deref() {
            Some(RawRef::Direct(_)) => return Ok(name),
            Some(RawRef::Symbolic(next)) => {
                name.clone_from(next);
                depth += 1;
            }
            None => return Ok(name),
        }
    }
}

fn append_reflog_entry(
    reflogs: &mut BTreeMap<String, Vec<ReflogEntry>>,
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
    reflogs.entry(name.to_owned()).or_default().push(entry);
}

/// Ref and reflog values before [`apply_transaction`], for rolling back on error.
pub(crate) struct ApplyRollbackGuard {
    touched: BTreeSet<String>,
    refs: BTreeMap<String, RawRef>,
    reflogs: BTreeMap<String, Vec<ReflogEntry>>,
}

/// Capture refs and reflogs `txn` may change so a failed apply can be undone cheaply.
#[must_use]
pub(crate) fn snapshot_for_apply_rollback(
    state: &RefBatchState,
    txn: &RefTransaction,
) -> ApplyRollbackGuard {
    let mut touched = ref_lock_names_for_updates(state, txn.updates());
    for update in txn.updates() {
        touched.insert(update.name.clone());
    }
    for rename in txn.case_only_renames() {
        touched.insert(rename.from.clone());
        touched.insert(rename.to.clone());
    }
    for rename in txn.reflog_renames() {
        touched.insert(rename.from.clone());
        touched.insert(rename.to.clone());
    }
    let mut refs = BTreeMap::new();
    let mut reflogs = BTreeMap::new();
    for name in &touched {
        if let Some(value) = state.refs.get(name) {
            refs.insert(name.clone(), value.clone());
        }
        if let Some(entries) = state.reflogs.get(name) {
            reflogs.insert(name.clone(), entries.clone());
        }
    }
    ApplyRollbackGuard {
        touched,
        refs,
        reflogs,
    }
}

/// Restore state saved by [`snapshot_for_apply_rollback`] after a failed apply.
pub(crate) fn rollback_failed_apply(state: &mut RefBatchState, backup: &ApplyRollbackGuard) {
    for name in &backup.touched {
        match backup.refs.get(name) {
            Some(value) => {
                state.refs.insert(name.clone(), value.clone());
            }
            None => {
                state.refs.remove(name);
            }
        }
        match backup.reflogs.get(name) {
            Some(entries) => {
                state.reflogs.insert(name.clone(), entries.clone());
            }
            None => {
                state.reflogs.remove(name);
            }
        }
    }
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
    reflog_old_oid_before_read(&RefsRead::Batch(state), update)
}

fn reflog_old_oid_before_read(refs: &RefsRead<'_, '_>, update: &RefUpdate) -> ObjectId {
    if let Some(oid) = update.reflog.as_ref().and_then(|log| log.old_oid) {
        return oid;
    }
    resolved_oid_at_name_read(refs, &update.name, update.flags).unwrap_or_else(|_| zero_oid())
}

fn reflog_new_oid_before(state: &RefBatchState, update: &RefUpdate) -> ObjectId {
    reflog_new_oid_before_read(&RefsRead::Batch(state), update)
}

fn reflog_new_oid_before_read(refs: &RefsRead<'_, '_>, update: &RefUpdate) -> ObjectId {
    if let Some(oid) = update.reflog.as_ref().and_then(|log| log.new_oid) {
        return oid;
    }
    if update.flags.log_only {
        if let Some(RawRef::Direct(oid)) = &update.new_value {
            return *oid;
        }
        return reflog_old_oid_before_read(refs, update);
    }
    match &update.new_value {
        None => zero_oid(),
        Some(RawRef::Direct(oid)) if should_deref_symref_update_read(refs, update) => *oid,
        Some(value) => {
            oid_for_reflog_value_read(refs, value, update.flags).unwrap_or_else(|_| zero_oid())
        }
    }
}

fn resolved_oid_at_name_read(
    refs: &RefsRead<'_, '_>,
    name: &str,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    let Some(value) = refs.get(name) else {
        return Ok(zero_oid());
    };
    oid_for_reflog_value_read(refs, value.as_ref(), flags)
}

fn oid_for_reflog_value_read(
    refs: &RefsRead<'_, '_>,
    value: &RawRef,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(_target) if flags.no_deref => Ok(zero_oid()),
        RawRef::Symbolic(target) => resolve_symbolic_read(refs, target),
    }
}

fn resolve_symbolic_read(refs: &RefsRead<'_, '_>, name: &str) -> Result<ObjectId, RefStoreError> {
    let mut current = name.to_owned();
    for _ in 0..SYMREF_MAXDEPTH {
        match refs.get(current.as_str()) {
            None => return Err(RefStoreError::not_found(&current)),
            Some(value) => match value.as_ref() {
                RawRef::Direct(oid) => return Ok(*oid),
                RawRef::Symbolic(target) => current = target.clone(),
            },
        }
    }
    Err(RefStoreError::SymrefLoop)
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
                    ..Default::default()
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
