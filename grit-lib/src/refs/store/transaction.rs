//! [`RefTransaction`] builder and validation.

use super::error::RefStoreError;
use super::types::{Expected, RawRef, RefCaseOnlyRename, RefReflogRename, RefUpdate};
use super::validation::validate_storable_refname;

/// Batch of ref updates prepared for [`super::RefStore::prepare`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RefTransaction {
    updates: Vec<RefUpdate>,
    reflog_renames: Vec<RefReflogRename>,
    case_only_renames: Vec<RefCaseOnlyRename>,
}

impl RefTransaction {
    /// Start an empty transaction.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
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
        if self
            .updates
            .iter()
            .any(|existing| existing.name == update.name)
        {
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

    /// Split into ref updates, reflog renames, and case-only ref renames.
    #[must_use]
    pub fn into_parts(self) -> (Vec<RefUpdate>, Vec<RefReflogRename>, Vec<RefCaseOnlyRename>) {
        (self.updates, self.reflog_renames, self.case_only_renames)
    }

    /// Backend-independent checks every [`super::RefStore::prepare`] runs before taking locks.
    ///
    /// Duplicate names cannot reach here because [`Self::push_update`] rejects them.
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
                name: "refs/heads/gone".to_owned(),
                new_value: None,
                expected: Expected::Any,
                reflog: None,
                flags: Default::default(),
            })
            .unwrap();
        assert!(txn.validate().is_ok());
    }

    #[test]
    fn expected_oid_requires_direct_match() {
        let direct = RawRef::Direct(oid(1));
        assert!(expected_matches(Some(&direct), &Expected::Oid(oid(1))));
        assert!(!expected_matches(Some(&direct), &Expected::Oid(oid(2))));
        assert!(!expected_matches(None, &Expected::Exists));
        assert!(expected_matches(None, &Expected::Missing));
    }
}
