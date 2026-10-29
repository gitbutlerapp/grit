//! [`RefTransaction`] builder and validation.

use std::collections::HashSet;

use super::error::RefStoreError;
use super::types::{Expected, RawRef, RefCaseOnlyRename, RefReflogRename, RefUpdate};
use super::validation::{
    validate_reflog_update, validate_storable_refname, validate_symref_target,
};

/// Batch of ref updates prepared for [`super::RefStore::prepare`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefTransaction {
    updates: Vec<RefUpdate>,
    names: HashSet<String>,
    reflog_renames: Vec<RefReflogRename>,
    case_only_renames: Vec<RefCaseOnlyRename>,
    initial: bool,
}

impl RefTransaction {
    /// Start an empty transaction.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start a transaction holding `updates`, checking for duplicate names in one pass.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::DuplicateUpdate`] for the first name queued twice.
    pub(crate) fn from_updates(updates: Vec<RefUpdate>) -> Result<Self, RefStoreError> {
        let mut names = HashSet::with_capacity(updates.len());
        for update in &updates {
            if !names.insert(update.name.clone()) {
                return Err(RefStoreError::DuplicateUpdate {
                    name: update.name.clone(),
                });
            }
        }
        Ok(Self {
            updates,
            names,
            ..Self::default()
        })
    }

    /// Append an update, rejecting duplicate ref names in the same batch.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::DuplicateUpdate`] when `update.name` was already queued.
    pub fn update(mut self, update: RefUpdate) -> Result<Self, RefStoreError> {
        self.push_update(update)?;
        Ok(self)
    }

    /// Append an update in place.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::DuplicateUpdate`] when `update.name` was already queued.
    pub fn push_update(&mut self, update: RefUpdate) -> Result<(), RefStoreError> {
        if !self.names.insert(update.name.clone()) {
            return Err(RefStoreError::DuplicateUpdate {
                name: update.name.clone(),
            });
        }
        self.updates.push(update);
        Ok(())
    }

    /// Borrow the queued updates.
    #[must_use]
    pub fn updates(&self) -> &[RefUpdate] {
        &self.updates
    }

    /// Consume the transaction and return the update list.
    #[must_use]
    pub fn into_updates(self) -> Vec<RefUpdate> {
        self.updates
    }

    /// Reflog rename operations bundled with this transaction.
    #[must_use]
    pub fn reflog_renames(&self) -> &[RefReflogRename] {
        &self.reflog_renames
    }

    /// Case-only ref renames bundled with this transaction.
    #[must_use]
    pub fn case_only_renames(&self) -> &[RefCaseOnlyRename] {
        &self.case_only_renames
    }

    /// Append a branch reflog rename to this transaction.
    #[must_use]
    pub fn with_reflog_rename(mut self, rename: RefReflogRename) -> Self {
        self.reflog_renames.push(rename);
        self
    }

    /// Append a case-only branch ref rename (files backend).
    #[must_use]
    pub fn with_case_only_rename(mut self, rename: RefCaseOnlyRename) -> Self {
        self.case_only_renames.push(rename);
        self
    }

    /// Mark this as the first ref transaction of a new repository, such as the remote refs a
    /// clone imports (Git's initial ref transaction).
    ///
    /// The files backend then writes the batch as one `packed-refs` rewrite, without a loose
    /// file or lock per ref, when every update creates a direct ref (no reflog, expectation
    /// [`Expected::Any`] or [`Expected::Missing`]) that does not exist yet in the common
    /// directory. Any other batch, and every other backend, commits as usual, so the flag
    /// never changes which refs end up stored, only how they are laid out.
    ///
    /// The packed write takes only `packed-refs.lock`: callers promise no other writer is
    /// creating the same refs, which holds for a repository nobody else knows about yet.
    #[must_use]
    pub fn initial(mut self) -> Self {
        self.initial = true;
        self
    }

    /// Whether [`Self::initial`] marked this transaction.
    #[must_use]
    pub fn is_initial(&self) -> bool {
        self.initial
    }

    /// Split into ref updates, reflog renames, and case-only ref renames.
    #[must_use]
    pub fn into_parts(self) -> (Vec<RefUpdate>, Vec<RefReflogRename>, Vec<RefCaseOnlyRename>) {
        (self.updates, self.reflog_renames, self.case_only_renames)
    }

    /// The ref a lock failure on this transaction is reported under: the first update, else
    /// the source of the first reflog or case-only rename, else `refs`.
    pub(crate) fn lock_name(&self) -> String {
        self.updates
            .first()
            .map(|u| &u.name)
            .or_else(|| self.reflog_renames.first().map(|r| &r.from))
            .or_else(|| self.case_only_renames.first().map(|r| &r.from))
            .map_or_else(|| "refs".to_owned(), Clone::clone)
    }

    /// Backend-independent checks every [`super::RefStore::prepare`] runs before taking locks.
    ///
    /// Duplicate names cannot reach here because [`Self::push_update`] and
    /// [`Self::from_updates`] reject them.
    pub(crate) fn validate(&self) -> Result<(), RefStoreError> {
        for update in &self.updates {
            validate_storable_refname(&update.name)?;
            if !update.flags.log_only
                && matches!(&update.new_value, Some(RawRef::Direct(oid)) if oid.is_zero())
            {
                return Err(RefStoreError::NullOid {
                    name: update.name.clone(),
                });
            }
            if let Some(ref log) = update.reflog {
                validate_reflog_update(log)?;
            }
            if let Some(RawRef::Symbolic(target)) = &update.new_value {
                validate_symref_target(&update.name, target)?;
            }
        }
        for rename in &self.reflog_renames {
            validate_reflog_update(&rename.append)?;
        }
        let renames = self
            .reflog_renames
            .iter()
            .map(|r| (&r.from, &r.to))
            .chain(self.case_only_renames.iter().map(|r| (&r.from, &r.to)));
        for (from, to) in renames {
            validate_storable_refname(from)?;
            validate_storable_refname(to)?;
        }
        Ok(())
    }
}

/// Whether `actual` satisfies `expected`.
#[must_use]
pub fn expected_matches(actual: Option<&RawRef>, expected: &Expected) -> bool {
    match expected {
        Expected::Any => true,
        Expected::Missing => actual.is_none(),
        Expected::Exists => actual.is_some(),
        Expected::Oid(want) => matches!(actual, Some(RawRef::Direct(got)) if got == want),
        Expected::Symref(want) => matches!(actual, Some(RawRef::Symbolic(got)) if got == want),
    }
}

#[cfg(test)]
mod tests {
    use crate::objects::ObjectId;

    use super::*;

    fn oid(byte: u8) -> ObjectId {
        let mut bytes = [0u8; 20];
        bytes[19] = byte;
        ObjectId::from_bytes(&bytes).expect("valid oid")
    }

    #[test]
    fn duplicate_names_rejected() {
        let txn = RefTransaction::new().update(RefUpdate {
            name: "refs/heads/a".to_owned(),
            new_value: Some(RawRef::Direct(oid(1))),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        });
        assert!(txn.is_ok());
        let err = txn
            .unwrap()
            .update(RefUpdate {
                name: "refs/heads/a".to_owned(),
                new_value: Some(RawRef::Direct(oid(2))),
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .unwrap_err();
        assert!(matches!(
            err,
            RefStoreError::DuplicateUpdate { name } if name == "refs/heads/a"
        ));
    }

    fn direct(name: &str, value: ObjectId) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Direct(value)),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        }
    }

    fn sample_log() -> super::super::ReflogUpdate {
        super::super::ReflogUpdate {
            identity: "A <a@example.com>".to_owned(),
            message: "rename".to_owned(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: None,
            new_oid: None,
        }
    }

    #[test]
    fn validate_rejects_unsafe_update_names() {
        for name in [
            "",
            "heads/x",
            "refs/heads/../x",
            "refs/heads/a b",
            "refs/heads/x/",
        ] {
            let err = RefTransaction::new()
                .update(direct(name, oid(1)))
                .unwrap()
                .validate()
                .unwrap_err();
            assert_eq!(
                err,
                RefStoreError::InvalidRefName {
                    name: name.to_owned()
                }
            );
        }
    }

    #[test]
    fn validate_rejects_unsafe_rename_sides() {
        let reflog = RefTransaction::new().with_reflog_rename(RefReflogRename {
            from: "refs/heads/ok".to_owned(),
            to: "refs/heads/x.lock".to_owned(),
            append: sample_log(),
        });
        assert_eq!(
            reflog.validate().unwrap_err(),
            RefStoreError::InvalidRefName {
                name: "refs/heads/x.lock".to_owned()
            }
        );
        let case_only = RefTransaction::new().with_case_only_rename(RefCaseOnlyRename {
            from: "refs/heads//x".to_owned(),
            to: "refs/heads/X".to_owned(),
        });
        assert_eq!(
            case_only.validate().unwrap_err(),
            RefStoreError::InvalidRefName {
                name: "refs/heads//x".to_owned()
            }
        );
    }

    #[test]
    fn validate_rejects_null_oid_unless_log_only() {
        let err = RefTransaction::new()
            .update(direct("refs/heads/z", ObjectId::zero()))
            .unwrap()
            .validate()
            .unwrap_err();
        assert_eq!(
            err,
            RefStoreError::NullOid {
                name: "refs/heads/z".to_owned()
            }
        );

        let mut log_only = direct("HEAD", ObjectId::zero());
        log_only.flags.log_only = true;
        assert!(RefTransaction::new()
            .update(log_only)
            .unwrap()
            .validate()
            .is_ok());
    }

    #[test]
    fn validate_rejects_forged_reflog_identity_and_message() {
        let mut update = direct("HEAD", oid(1));
        update.flags.log_only = true;
        update.reflog = Some(super::super::ReflogUpdate {
            identity: "Evil <a@b>\nforged".to_owned(),
            message: "ok".to_owned(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: None,
            new_oid: None,
        });
        assert!(matches!(
            RefTransaction::new()
                .update(update)
                .unwrap()
                .validate()
                .unwrap_err(),
            RefStoreError::InvalidReflog(_)
        ));

        let mut update = direct("HEAD", oid(1));
        update.flags.log_only = true;
        update.reflog = Some(super::super::ReflogUpdate {
            identity: "A <a@example.com>".to_owned(),
            message: "bad\nline".to_owned(),
            time: time::OffsetDateTime::UNIX_EPOCH,
            old_oid: None,
            new_oid: None,
        });
        assert!(matches!(
            RefTransaction::new()
                .update(update)
                .unwrap()
                .validate()
                .unwrap_err(),
            RefStoreError::InvalidReflog(_)
        ));

        let txn = RefTransaction::new().with_reflog_rename(RefReflogRename {
            from: "refs/heads/a".to_owned(),
            to: "refs/heads/b".to_owned(),
            append: super::super::ReflogUpdate {
                identity: "Evil <a@b>\nforged".to_owned(),
                message: "rename".to_owned(),
                time: time::OffsetDateTime::UNIX_EPOCH,
                old_oid: None,
                new_oid: None,
            },
        });
        assert!(matches!(
            txn.validate().unwrap_err(),
            RefStoreError::InvalidReflog(_)
        ));
    }

    #[test]
    fn validate_accepts_pseudorefs_symrefs_and_deletes() {
        let txn = RefTransaction::new()
            .update(direct("ORIG_HEAD", oid(1)))
            .unwrap()
            .update(RefUpdate {
                name: "HEAD".to_owned(),
                new_value: Some(RawRef::Symbolic("refs/heads/main".to_owned())),
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .unwrap()
            .update(RefUpdate {
                name: "refs/heads/alias".to_owned(),
                new_value: Some(RawRef::Symbolic("refs/heads/not-yet".to_owned())),
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .unwrap()
            .update(RefUpdate {
                name: "FETCH_HEAD".to_owned(),
                new_value: Some(RawRef::Symbolic("ORIG_HEAD".to_owned())),
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .unwrap()
            .update(RefUpdate {
                name: "refs/heads/gone".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .unwrap();
        assert!(txn.validate().is_ok());
    }

    fn symref(name: &str, target: &str) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Symbolic(target.to_owned())),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        }
    }

    fn invalid_symref(name: &str, target: &str) -> RefStoreError {
        RefStoreError::InvalidSymrefTarget {
            name: name.to_owned(),
            target: target.to_owned(),
        }
    }

    #[test]
    fn validate_rejects_invalid_symref_targets() {
        for (name, target) in [
            ("HEAD", "foo"),
            ("refs/heads/sym1", "foo"),
            ("refs/heads/loop", "refs/heads/loop"),
            ("HEAD", "refs/heads/a..b"),
        ] {
            let err = RefTransaction::new()
                .update(symref(name, target))
                .unwrap()
                .validate()
                .unwrap_err();
            assert_eq!(err, invalid_symref(name, target), "{name} -> {target}");
        }
    }

    #[test]
    fn from_updates_rejects_a_name_queued_twice() {
        let update = |name: &str| RefUpdate {
            name: name.to_owned(),
            new_value: None,
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        };
        let txn =
            RefTransaction::from_updates(vec![update("refs/heads/a"), update("refs/heads/b")])
                .expect("distinct names");
        assert_eq!(txn.updates().len(), 2);
        let err = RefTransaction::from_updates(vec![
            update("refs/heads/a"),
            update("refs/heads/b"),
            update("refs/heads/a"),
        ])
        .unwrap_err();
        assert!(matches!(
            err,
            RefStoreError::DuplicateUpdate { name } if name == "refs/heads/a"
        ));
    }

    #[test]
    fn expected_oid_requires_direct_match() {
        let direct = RawRef::Direct(oid(1));
        assert!(expected_matches(Some(&direct), &Expected::Oid(oid(1))));
        assert!(!expected_matches(Some(&direct), &Expected::Oid(oid(2))));
        assert!(!expected_matches(None, &Expected::Exists));
        assert!(expected_matches(None, &Expected::Missing));
    }

    #[test]
    fn from_updates_then_push_update_rejects_duplicate_name() {
        let update = |name: &str| RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Direct(oid(1))),
            expected: Expected::Any,
            reflog: None,
            flags: Default::default(),
        };
        let mut txn =
            RefTransaction::from_updates(vec![update("refs/heads/a"), update("refs/heads/b")])
                .expect("distinct names");
        let err = txn
            .push_update(update("refs/heads/a"))
            .expect_err("duplicate after from_updates");
        assert!(matches!(
            err,
            RefStoreError::DuplicateUpdate { name } if name == "refs/heads/a"
        ));
        assert_eq!(txn.updates().len(), 2);
    }

    fn push_unique_updates(n: u32) -> std::time::Duration {
        let mut txn = RefTransaction::new();
        let start = std::time::Instant::now();
        for i in 0..n {
            txn.push_update(RefUpdate {
                name: format!("refs/heads/b{i:07}"),
                new_value: Some(RawRef::Direct(oid((i % 256) as u8))),
                expected: Expected::Missing,
                reflog: None,
                flags: Default::default(),
            })
            .expect("unique name");
        }
        start.elapsed()
    }

    #[test]
    fn push_update_batch_build_scales_linearly() {
        let small = push_unique_updates(2_000);
        let large = push_unique_updates(8_000);
        let ratio = large.as_secs_f64() / small.as_secs_f64().max(1e-9);
        assert!(
            ratio < 12.0,
            "expected roughly linear scaling (~4x for 4x refs), got {ratio:.1}x \
             (small={small:?}, large={large:?})"
        );
    }

    #[test]
    #[ignore = "release-only sanity check for large batches (#2281); cargo test -p grit-lib --release push_update_100k -- --ignored --exact"]
    fn push_update_100k_build_under_one_second() {
        let elapsed = push_unique_updates(100_000);
        assert!(
            elapsed.as_secs() < 3,
            "100k push_update build took {elapsed:?} (expected well under 1s in release)"
        );
    }
}
