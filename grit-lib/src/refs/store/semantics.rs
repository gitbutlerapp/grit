//! Shared ref-update semantics for in-memory and reftable [`super::RefStore`] backends.

use std::collections::BTreeMap;

use crate::diff::zero_oid;
use crate::objects::ObjectId;
use crate::refs::SYMREF_MAXDEPTH;

use super::error::RefStoreError;
use super::transaction::expected_matches;
use super::{RawRef, RefUpdate, RefUpdateFlags};

type StoreResult<T> = std::result::Result<T, RefStoreError>;

/// Validate CAS expectations and apply updates to an in-memory ref map (prepare simulation).
pub fn simulate_batch_apply(
    refs: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
) -> StoreResult<()> {
    let mut trial = refs.clone();
    for update in updates {
        let actual = trial.get(&update.name);
        if !expected_matches(actual, &update.expected) {
            return Err(RefStoreError::ExpectedMismatch {
                name: update.name.clone(),
                expected: update.expected.clone(),
                actual: actual.cloned(),
            });
        }
        apply_update_to_map(&mut trial, refs, update)?;
    }
    Ok(())
}

pub(crate) fn apply_update_to_map(
    trial: &mut BTreeMap<String, RawRef>,
    baseline: &BTreeMap<String, RawRef>,
    update: &RefUpdate,
) -> StoreResult<()> {
    let reflog_oids = update.reflog.as_ref().map(|_| {
        (
            reflog_old_oid_before(trial, baseline, update),
            reflog_new_oid_before(trial, baseline, update),
        )
    });

    apply_ref_change(trial, update)?;

    if let (Some(_log), Some((_old, _new))) = (&update.reflog, reflog_oids) {
        // Reflog presence is validated; reftable/memory backends record logs separately.
    }
    Ok(())
}

fn apply_ref_change(trial: &mut BTreeMap<String, RawRef>, update: &RefUpdate) -> StoreResult<()> {
    if update.flags.log_only {
        return Ok(());
    }
    match &update.new_value {
        None => {
            if should_deref_symref(trial, update) {
                let target = symref_peel_write_target(trial, &update.name)?;
                trial.remove(&target);
            } else {
                trial.remove(&update.name);
            }
        }
        Some(new_value) => {
            if should_deref_symref_update(trial, update) {
                let RawRef::Direct(oid) = new_value else {
                    return Err(RefStoreError::Corrupt(
                        "deref update requires direct oid".to_owned(),
                    ));
                };
                let target = symref_peel_write_target(trial, &update.name)?;
                trial.insert(target, RawRef::Direct(*oid));
            } else {
                trial.insert(update.name.clone(), new_value.clone());
            }
        }
    }
    Ok(())
}

fn should_deref_symref(refs: &BTreeMap<String, RawRef>, update: &RefUpdate) -> bool {
    !update.flags.no_deref && matches!(refs.get(&update.name), Some(RawRef::Symbolic(_)))
}

fn should_deref_symref_update(refs: &BTreeMap<String, RawRef>, update: &RefUpdate) -> bool {
    should_deref_symref(refs, update)
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

pub(crate) fn reflog_old_oid_before(
    trial: &BTreeMap<String, RawRef>,
    baseline: &BTreeMap<String, RawRef>,
    update: &RefUpdate,
) -> ObjectId {
    resolved_oid_at_name(trial, baseline, &update.name, update.flags).unwrap_or_else(|_| zero_oid())
}

pub(crate) fn reflog_new_oid_before(
    trial: &BTreeMap<String, RawRef>,
    baseline: &BTreeMap<String, RawRef>,
    update: &RefUpdate,
) -> ObjectId {
    if update.flags.log_only {
        return reflog_old_oid_before(trial, baseline, update);
    }
    match &update.new_value {
        None => zero_oid(),
        Some(RawRef::Direct(oid)) if should_deref_symref_update(trial, update) => *oid,
        Some(value) => oid_for_reflog_value(trial, baseline, value, update.flags)
            .unwrap_or_else(|_| zero_oid()),
    }
}

fn resolved_oid_at_name(
    trial: &BTreeMap<String, RawRef>,
    baseline: &BTreeMap<String, RawRef>,
    name: &str,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    let map = if trial.contains_key(name) {
        trial
    } else {
        baseline
    };
    let Some(value) = map.get(name) else {
        return Ok(zero_oid());
    };
    oid_for_reflog_value(trial, baseline, value, flags)
}

fn oid_for_reflog_value(
    trial: &BTreeMap<String, RawRef>,
    baseline: &BTreeMap<String, RawRef>,
    value: &RawRef,
    flags: RefUpdateFlags,
) -> StoreResult<ObjectId> {
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(_target) if flags.no_deref => Ok(zero_oid()),
        RawRef::Symbolic(target) => resolve_map(trial, baseline, target, 0),
    }
}

fn resolve_map(
    trial: &BTreeMap<String, RawRef>,
    baseline: &BTreeMap<String, RawRef>,
    name: &str,
    depth: usize,
) -> StoreResult<ObjectId> {
    if depth >= SYMREF_MAXDEPTH {
        return Err(RefStoreError::SymrefLoop);
    }
    let map = if trial.contains_key(name) {
        trial
    } else {
        baseline
    };
    let Some(value) = map.get(name) else {
        return Err(RefStoreError::Corrupt(format!("ref not found: {name}")));
    };
    match value {
        RawRef::Direct(oid) => Ok(*oid),
        RawRef::Symbolic(target) => resolve_map(trial, baseline, target, depth + 1),
    }
}

/// Storage refname and optional ref value for one update in a reftable transaction.
pub(crate) struct ReftableWriteTarget {
    pub storage_refname: String,
    pub value: Option<crate::reftable::RefValue>,
}

/// Map a logical [`RefUpdate`] to the reftable storage refname and value.
pub(crate) fn reftable_write_target(
    refs: &BTreeMap<String, RawRef>,
    git_dir: &std::path::Path,
    update: &RefUpdate,
) -> StoreResult<ReftableWriteTarget> {
    let (_store, storage_refname) =
        crate::reftable::reftable_storage_location(git_dir, &update.name);
    if update.flags.log_only {
        return Ok(ReftableWriteTarget {
            storage_refname,
            value: None,
        });
    }
    let value = match &update.new_value {
        None => {
            if should_deref_symref(refs, update) {
                let target = symref_peel_write_target(refs, &update.name)?;
                let (_s, storage) = crate::reftable::reftable_storage_location(git_dir, &target);
                return Ok(ReftableWriteTarget {
                    storage_refname: storage,
                    value: Some(crate::reftable::RefValue::Deletion),
                });
            }
            Some(crate::reftable::RefValue::Deletion)
        }
        Some(RawRef::Direct(oid)) if should_deref_symref_update(refs, update) => {
            let target = symref_peel_write_target(refs, &update.name)?;
            let (_s, storage) = crate::reftable::reftable_storage_location(git_dir, &target);
            return Ok(ReftableWriteTarget {
                storage_refname: storage,
                value: Some(crate::reftable::RefValue::Val1(*oid)),
            });
        }
        Some(RawRef::Direct(oid)) => Some(crate::reftable::RefValue::Val1(*oid)),
        Some(RawRef::Symbolic(target)) => Some(crate::reftable::RefValue::Symref(target.clone())),
    };
    Ok(ReftableWriteTarget {
        storage_refname,
        value,
    })
}

/// Whether `identity` already ends with Git's reflog `<unix> <±HHMM>` suffix.
#[must_use]
pub(crate) fn reflog_identity_has_timestamp(identity: &str) -> bool {
    let trimmed = identity.trim_end();
    let Some((before_tz, tz)) = trimmed.rsplit_once(' ') else {
        return false;
    };
    if tz.len() != 5 {
        return false;
    }
    let Some(sign) = tz.chars().next() else {
        return false;
    };
    if sign != '+' && sign != '-' {
        return false;
    }
    if !tz[1..].chars().all(|c| c.is_ascii_digit()) {
        return false;
    }
    let Some((name_email, unix)) = before_tz.rsplit_once(' ') else {
        return false;
    };
    !name_email.is_empty() && !unix.is_empty() && unix.chars().all(|c| c.is_ascii_digit())
}

pub(crate) fn format_reflog_identity(identity: &str, time: time::OffsetDateTime) -> String {
    if reflog_identity_has_timestamp(identity) {
        return identity.to_owned();
    }
    let offset = time.offset().whole_seconds();
    let hours = offset / 3600;
    let minutes = (offset.abs() % 3600) / 60;
    format!(
        "{identity} {} {:+03}{:02}",
        time.unix_timestamp(),
        hours,
        minutes
    )
}

#[cfg(test)]
mod tests {
    use super::{format_reflog_identity, reflog_identity_has_timestamp};
    use time::OffsetDateTime;

    #[test]
    fn reflog_identity_digits_in_name_still_need_timestamp() {
        assert!(!reflog_identity_has_timestamp("Alice2 <alice@example.com>"));
        let time = OffsetDateTime::from_unix_timestamp(1_700_000_000).unwrap();
        let formatted = format_reflog_identity("Alice2 <alice@example.com>", time);
        assert!(formatted.contains("1700000000"));
    }

    #[test]
    fn reflog_identity_with_existing_suffix_is_preserved() {
        let existing = "Bob <bob@example.com> 1234567890 +0000";
        assert!(reflog_identity_has_timestamp(existing));
        let time = OffsetDateTime::from_unix_timestamp(9).unwrap();
        assert_eq!(format_reflog_identity(existing, time), existing);
    }
}
