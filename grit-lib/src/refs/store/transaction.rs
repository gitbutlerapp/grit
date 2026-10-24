//! [`RefTransaction`] builder and validation.

use std::collections::HashSet;

use super::error::RefStoreError;
use super::types::{Expected, RawRef, RefCaseOnlyRename, RefReflogRename, RefUpdate};

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

    /// Start a transaction holding `updates`, checking for duplicate names in one pass.
    ///
    /// # Errors
    ///
    /// Returns [`RefStoreError::DuplicateUpdate`] for the first name queued twice.
    pub(crate) fn from_updates(updates: Vec<RefUpdate>) -> Result<Self, RefStoreError> {
        Self::validate_no_duplicates(&updates)?;
        Ok(Self {
            updates,
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

    pub(crate) fn validate_no_duplicates(updates: &[RefUpdate]) -> Result<(), RefStoreError> {
        let mut seen = HashSet::new();
        for update in updates {
            if !seen.insert(&update.name) {
                return Err(RefStoreError::DuplicateUpdate {
                    name: update.name.clone(),
                });
            }
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
}
