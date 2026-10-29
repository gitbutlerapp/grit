//! Directory/file refname conflict checks for in-memory stores.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ops::Bound;

use crate::ident::{parse_author_arg, parse_signature, SignatureError};
use crate::refs::RefnameUnavailable;

use super::error::RefStoreError;
use super::semantics::reflog_identity_has_timestamp;
use super::RawRef;
use super::RefUpdate;
use super::ReflogUpdate;

/// Reject ref names that must not touch the filesystem (path traversal, illegal `refs/` shape).
pub fn validate_storable_refname(name: &str) -> Result<(), RefStoreError> {
    crate::refs::ensure_refname_safe_for_storage(name).map_err(|_| RefStoreError::InvalidRefName {
        name: name.to_owned(),
    })
}

/// Reject a symbolic ref value before any lock or target lookup.
///
/// The target must be safe to store (same rules as an update name). `HEAD` may only point
/// under `refs/`. A ref may not symbolically point at itself.
///
/// # Errors
///
/// Returns [`RefStoreError::InvalidSymrefTarget`] naming the ref and target.
pub fn validate_symref_target(name: &str, target: &str) -> Result<(), RefStoreError> {
    let err = || RefStoreError::InvalidSymrefTarget {
        name: name.to_owned(),
        target: target.to_owned(),
    };
    if crate::refs::ensure_refname_safe_for_storage(target).is_err() {
        return Err(err());
    }
    if name == "HEAD" && !target.starts_with("refs/") {
        return Err(err());
    }
    if target == name {
        return Err(err());
    }
    Ok(())
}

/// Reject reflog message bytes that would split a physical log line.
pub fn validate_reflog_message(message: &str) -> Result<(), RefStoreError> {
    if message.bytes().any(|b| b == b'\n' || b == b'\r') {
        return Err(RefStoreError::InvalidReflog(SignatureError::Malformed));
    }
    Ok(())
}

/// Reject reflog identity strings that would forge or corrupt a log line.
pub fn validate_reflog_identity_string(identity: &str) -> Result<(), RefStoreError> {
    if identity
        .bytes()
        .any(|b| b == b'\n' || b == b'\r' || (b < 0x20 && b != b'\t'))
    {
        return Err(RefStoreError::InvalidReflog(SignatureError::Malformed));
    }
    if reflog_identity_has_timestamp(identity) {
        parse_signature(identity).ok_or(RefStoreError::InvalidReflog(SignatureError::Malformed))?;
    } else {
        parse_author_arg(identity)
            .map_err(|_| RefStoreError::InvalidReflog(SignatureError::Malformed))?;
    }
    Ok(())
}

/// Reject reflog metadata bundled with a ref update or rename.
pub fn validate_reflog_update(log: &ReflogUpdate) -> Result<(), RefStoreError> {
    validate_reflog_identity_string(&log.identity)?;
    validate_reflog_message(&log.message)?;
    Ok(())
}

/// Verify batch-internal D/F conflicts (Git `refs_verify_refnames_available` between extras):
/// no name of `updates` may be a `/`-separated ancestor of another.
///
/// `names` holds every name of `updates`. Each name costs one set probe per ancestor plus one
/// range probe for descendants, so a batch of `n` refs is checked in `O(n log n)`. The range
/// probe starts at `<name>/`, which also finds a child separated from its parent by names
/// holding a byte that sorts before `/` (`a/b`, `a!`, `a/b/c`).
fn verify_same_batch_conflicts(
    updates: &[RefUpdate],
    names: &BTreeSet<&str>,
) -> Result<(), RefnameUnavailable> {
    let mut dir = String::new();
    for update in updates {
        let name = update.name.as_str();
        if let Some(parent) = name
            .match_indices('/')
            .map(|(slash, _)| &name[..slash])
            .find(|ancestor| names.contains(ancestor))
        {
            return Err(RefnameUnavailable::SameBatch {
                refname: name.to_owned(),
                other: parent.to_owned(),
            });
        }
        dir.clear();
        dir.push_str(name);
        dir.push('/');
        if let Some(child) = find_descendant_in_sorted(&dir, names) {
            return Err(RefnameUnavailable::SameBatch {
                refname: (*child).to_owned(),
                other: name.to_owned(),
            });
        }
    }
    Ok(())
}

fn find_descendant_in_sorted<'a, S>(
    dirname_with_slash: &str,
    names: &'a BTreeSet<S>,
) -> Option<&'a S>
where
    S: Borrow<str> + Ord,
{
    let start = names
        .range::<str, _>((Bound::Included(dirname_with_slash), Bound::Unbounded))
        .next()?;
    start
        .borrow()
        .starts_with(dirname_with_slash)
        .then_some(start)
}

/// Verify that each *new* ref in `updates` can be created without conflicting with `existing`
/// or other batch items. Deletes in the batch do not suppress conflicts (Git t1404).
pub fn verify_create_conflicts(
    existing: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
) -> Result<(), RefnameUnavailable> {
    let extras: BTreeSet<&str> = updates.iter().map(|u| u.name.as_str()).collect();
    verify_same_batch_conflicts(updates, &extras)?;

    for update in updates {
        if update.new_value.is_none() || update.flags.log_only {
            continue;
        }
        if existing.contains_key(&update.name) {
            continue;
        }
        verify_name_available_for_create_in_map(&update.name, existing, &extras)?;
    }
    Ok(())
}

/// Whether `refname` can be created against a sorted ref map without scanning every key.
fn verify_name_available_for_create_in_map(
    refname: &str,
    existing: &BTreeMap<String, RawRef>,
    extras: &BTreeSet<&str>,
) -> Result<(), RefnameUnavailable> {
    let segments: Vec<&str> = refname.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() > 1 {
        let mut dirname = String::new();
        for part in &segments[..segments.len() - 1] {
            if !dirname.is_empty() {
                dirname.push('/');
            }
            dirname.push_str(part);
            if existing.contains_key(&dirname) {
                return Err(RefnameUnavailable::AncestorExists {
                    blocking: dirname.clone(),
                    new_ref: refname.to_owned(),
                });
            }
            if extras.contains(dirname.as_str()) {
                return Err(RefnameUnavailable::SameBatch {
                    refname: refname.to_owned(),
                    other: dirname.clone(),
                });
            }
        }
    }

    let mut leaf_dir = String::with_capacity(refname.len() + 1);
    leaf_dir.push_str(refname);
    leaf_dir.push('/');

    if let Some((blocking, _)) = existing
        .range::<str, _>((Bound::Included(leaf_dir.as_str()), Bound::Unbounded))
        .next()
        .filter(|(name, _)| name.starts_with(leaf_dir.as_str()))
    {
        return Err(RefnameUnavailable::DescendantExists {
            blocking: blocking.clone(),
            new_ref: refname.to_owned(),
        });
    }
    if let Some(blocking) = find_descendant_in_sorted(&leaf_dir, extras) {
        return Err(RefnameUnavailable::SameBatch {
            refname: refname.to_owned(),
            other: (*blocking).to_owned(),
        });
    }

    Ok(())
}

/// Whether `refname` can be created when `existing` holds the stored ref names near it (at
/// least its ancestors and descendants) and `extras` the names of its batch.
pub(super) fn verify_name_available_for_create<E, X>(
    refname: &str,
    existing: &BTreeSet<E>,
    extras: &BTreeSet<X>,
) -> Result<(), RefnameUnavailable>
where
    E: Borrow<str> + Ord,
    X: Borrow<str> + Ord,
{
    let segments: Vec<&str> = refname.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() > 1 {
        let mut dirname = String::new();
        for part in &segments[..segments.len() - 1] {
            if !dirname.is_empty() {
                dirname.push('/');
            }
            dirname.push_str(part);
            if existing.contains(dirname.as_str()) {
                return Err(RefnameUnavailable::AncestorExists {
                    blocking: dirname.clone(),
                    new_ref: refname.to_owned(),
                });
            }
            if extras.contains(dirname.as_str()) {
                return Err(RefnameUnavailable::SameBatch {
                    refname: refname.to_owned(),
                    other: dirname.clone(),
                });
            }
        }
    }

    let mut leaf_dir = String::with_capacity(refname.len() + 1);
    leaf_dir.push_str(refname);
    leaf_dir.push('/');

    if let Some(blocking) = find_descendant_in_sorted(&leaf_dir, existing) {
        return Err(RefnameUnavailable::DescendantExists {
            blocking: blocking.borrow().to_owned(),
            new_ref: refname.to_owned(),
        });
    }
    if let Some(blocking) = find_descendant_in_sorted(&leaf_dir, extras) {
        return Err(RefnameUnavailable::SameBatch {
            refname: refname.to_owned(),
            other: blocking.borrow().to_owned(),
        });
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectId;
    use crate::refs::store::{Expected, RefUpdateFlags};

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(&[byte; 20]).expect("oid")
    }

    fn create(name: &str) -> RefUpdate {
        RefUpdate {
            name: name.to_owned(),
            new_value: Some(RawRef::Direct(oid(1))),
            expected: Expected::Missing,
            reflog: None,
            flags: RefUpdateFlags::default(),
        }
    }

    fn delete(name: &str) -> RefUpdate {
        RefUpdate {
            new_value: None,
            expected: Expected::Any,
            ..create(name)
        }
    }

    fn siblings(n: usize) -> Vec<RefUpdate> {
        (0..n)
            .map(|i| create(&format!("refs/remotes/origin/b/n{i}")))
            .collect()
    }

    #[test]
    fn many_sibling_creates_are_accepted() {
        let updates = siblings(20_000);
        verify_create_conflicts(&BTreeMap::new(), &updates).expect("siblings never conflict");
    }

    #[test]
    fn parent_and_child_split_by_a_name_sorting_before_slash_conflict() {
        // `x!` sorts between `x` and `x/y`, so only a probe at `x/` finds the pair.
        for order in [["x", "x!", "x/y"], ["x/y", "x!", "x"]] {
            let mut updates = siblings(3_000);
            updates.extend(order.map(|leaf| create(&format!("refs/heads/{leaf}"))));
            let err = verify_create_conflicts(&BTreeMap::new(), &updates)
                .expect_err("parent and child in one batch");
            assert_eq!(
                err,
                RefnameUnavailable::SameBatch {
                    refname: "refs/heads/x/y".to_owned(),
                    other: "refs/heads/x".to_owned(),
                },
                "{order:?}"
            );
        }
    }

    #[test]
    fn deleting_a_parent_does_not_free_its_name_for_a_child_in_the_same_batch() {
        let existing: BTreeMap<String, RawRef> =
            [("refs/heads/a".to_owned(), RawRef::Direct(oid(2)))].into();
        let updates = vec![delete("refs/heads/a"), create("refs/heads/a/b")];
        assert_eq!(
            verify_create_conflicts(&existing, &updates),
            Err(RefnameUnavailable::SameBatch {
                refname: "refs/heads/a/b".to_owned(),
                other: "refs/heads/a".to_owned(),
            })
        );
    }

    #[test]
    fn create_next_to_stored_refs_reports_ancestor_and_descendant() {
        let existing: BTreeMap<String, RawRef> = [
            ("refs/heads/p".to_owned(), RawRef::Direct(oid(2))),
            ("refs/heads/q/r".to_owned(), RawRef::Direct(oid(2))),
        ]
        .into();
        assert_eq!(
            verify_create_conflicts(&existing, &[create("refs/heads/p/c")]),
            Err(RefnameUnavailable::AncestorExists {
                blocking: "refs/heads/p".to_owned(),
                new_ref: "refs/heads/p/c".to_owned(),
            })
        );
        assert_eq!(
            verify_create_conflicts(&existing, &[create("refs/heads/q")]),
            Err(RefnameUnavailable::DescendantExists {
                blocking: "refs/heads/q/r".to_owned(),
                new_ref: "refs/heads/q".to_owned(),
            })
        );
        verify_create_conflicts(&existing, &[create("refs/heads/q!")]).expect("no conflict");
    }

    #[test]
    fn large_unrelated_map_still_reports_df_conflicts() {
        let mut existing: BTreeMap<String, RawRef> = BTreeMap::new();
        for i in 0..25_000 {
            existing.insert(
                format!("refs/heads/unrelated/b{i:05}"),
                RawRef::Direct(oid(2)),
            );
        }
        existing.insert("refs/heads/p".to_owned(), RawRef::Direct(oid(2)));
        existing.insert("refs/heads/q/r".to_owned(), RawRef::Direct(oid(2)));
        assert_eq!(
            verify_create_conflicts(&existing, &[create("refs/heads/p/c")]),
            Err(RefnameUnavailable::AncestorExists {
                blocking: "refs/heads/p".to_owned(),
                new_ref: "refs/heads/p/c".to_owned(),
            })
        );
        assert_eq!(
            verify_create_conflicts(&existing, &[create("refs/heads/q")]),
            Err(RefnameUnavailable::DescendantExists {
                blocking: "refs/heads/q/r".to_owned(),
                new_ref: "refs/heads/q".to_owned(),
            })
        );
        verify_create_conflicts(&existing, &[create("refs/heads/q!")]).expect("no conflict");
    }
}
