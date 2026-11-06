//! Directory/file refname conflict checks for in-memory stores.

use std::borrow::Borrow;
use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::ops::Bound;

use crate::ident::{parse_author_arg, parse_signature, SignatureError};
use crate::refs::RefnameUnavailable;

use super::error::RefStoreError;
use super::semantics::reflog_identity_has_timestamp;
use super::{RawRef, RefUpdate, ReflogUpdate};

/// Reject ref names that must not touch the filesystem (path traversal, illegal `refs/` shape).
pub fn validate_storable_refname(ref_name: &str) -> Result<(), RefStoreError> {
    crate::refs::ensure_refname_safe_for_storage(ref_name).map_err(|_| {
        RefStoreError::InvalidRefName {
            name: ref_name.to_owned(),
        }
    })
}

/// Reject a symbolic ref value before any lock or target lookup.
///
/// `HEAD` must point at a name safe to store under `refs/`. Other refs use Git's
/// one-level symbolic target rules (`--allow-onelevel`). A ref may not symbolically point at
/// itself.
///
/// # Errors
///
/// Returns [`RefStoreError::InvalidSymrefTarget`] naming the ref and target.
pub fn validate_symref_target(ref_name: &str, target: &str) -> Result<(), RefStoreError> {
    let err = || RefStoreError::InvalidSymrefTarget {
        name: ref_name.to_owned(),
        target: target.to_owned(),
    };
    if target == ref_name {
        return Err(err());
    }
    if ref_name == "HEAD" {
        if !target.starts_with("refs/")
            || crate::refs::ensure_refname_safe_for_storage(target).is_err()
        {
            return Err(err());
        }
    } else if !crate::refs::is_valid_advertised_symref_target(target) {
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
        if existing.contains_key(update.name.as_str()) {
            continue;
        }
        verify_name_available_for_create_in_map(update.name.as_str(), existing, &extras)?;
    }
    Ok(())
}

/// Whether two ref names cannot coexist when `core.ignorecase` is enabled: they share a path
/// component slot that matches under ASCII case folding but is spelled differently (including
/// parallel trees such as `refs/heads/Foo/bar` vs `refs/heads/foo/baz`).
#[must_use]
pub(crate) fn ascii_casefold_ref_paths_collide(a: &str, b: &str) -> bool {
    if a == b {
        return false;
    }
    let mut a_parts = a.split('/').filter(|s| !s.is_empty());
    let mut b_parts = b.split('/').filter(|s| !s.is_empty());
    loop {
        match (a_parts.next(), b_parts.next()) {
            (Some(x), Some(y)) => {
                if !x.eq_ignore_ascii_case(y) {
                    return false;
                }
                if x != y {
                    return true;
                }
            }
            (Some(_), None) | (None, Some(_)) => return false,
            (None, None) => return false,
        }
    }
}

/// When `ignore_case`, reject each create whose name matches an existing ref under ASCII
/// case folding (`core.ignorecase` on macOS/Windows).
///
/// Same-batch ASCII case variants are allowed (initial clone stores them in `packed-refs`).
///
/// # Errors
///
/// [`RefStoreError::CreateExists`] when a conflicting stored name is found.
pub fn verify_create_casefold_against_existing(
    ignore_case: bool,
    existing: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
) -> Result<(), RefStoreError> {
    if !ignore_case {
        return Ok(());
    }
    for update in updates {
        if update.new_value.is_none() || update.flags.log_only {
            continue;
        }
        if existing.contains_key(update.name.as_str()) {
            continue;
        }
        let name = update.name.as_str();
        if existing
            .keys()
            .any(|other| other.as_str() != name && other.eq_ignore_ascii_case(name))
        {
            return Err(RefStoreError::CreateExists {
                name: update.name.as_str().to_owned(),
            });
        }
    }
    Ok(())
}

/// When `ignore_case`, reject each create whose name matches an existing ref or another
/// create in `updates` under ASCII case folding (`core.ignorecase` on macOS/Windows).
///
/// Case-sensitive repositories pass `ignore_case: false` and this returns immediately.
///
/// # Errors
///
/// [`RefStoreError::CreateExists`] when a conflicting stored name is found.
/// [`RefStoreError::CaseConflict`] when two creates in `updates` differ only by case.
pub fn verify_create_casefold_conflicts(
    ignore_case: bool,
    existing: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
) -> Result<(), RefStoreError> {
    if !ignore_case {
        return Ok(());
    }
    verify_create_casefold_against_existing(ignore_case, existing, updates)?;
    verify_create_casefold_same_batch(updates)
}

/// Reject two creates in one batch whose names differ only by ASCII case.
///
/// # Errors
///
/// [`RefStoreError::CaseConflict`] when two creates in `updates` differ only by case.
pub fn verify_create_casefold_same_batch(updates: &[RefUpdate]) -> Result<(), RefStoreError> {
    let extras: BTreeSet<&str> = updates.iter().map(|u| u.name.as_str()).collect();
    for update in updates {
        if update.new_value.is_none() || update.flags.log_only {
            continue;
        }
        let name = update.name.as_str();
        if let Some(other) = extras
            .iter()
            .copied()
            .find(|other| *other != name && other.eq_ignore_ascii_case(name))
        {
            return Err(RefStoreError::CaseConflict {
                name: update.name.as_str().to_owned(),
                other: other.to_owned(),
            });
        }
    }
    Ok(())
}

/// When `ignore_case`, reject each create whose name matches a stored ref under ASCII case
/// folding, using `existing` to look up collisions without loading every ref value.
///
/// # Errors
///
/// [`RefStoreError::CreateExists`] or [`RefStoreError::CaseConflict`].
pub fn verify_create_casefold_with_lookup(
    ignore_case: bool,
    skip_exact: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
    mut existing: impl FnMut(&str) -> Result<Option<String>, RefStoreError>,
) -> Result<(), RefStoreError> {
    if !ignore_case {
        return Ok(());
    }
    for update in updates {
        if update.new_value.is_none() || update.flags.log_only {
            continue;
        }
        if skip_exact.contains_key(update.name.as_str()) {
            continue;
        }
        let name = update.name.as_str();
        if existing(name)?.is_some() {
            return Err(RefStoreError::CreateExists {
                name: update.name.as_str().to_owned(),
            });
        }
    }
    verify_create_casefold_same_batch(updates)
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
    use crate::refs::store::{Expected, RefName, RefUpdateFlags};

    fn oid(byte: u8) -> ObjectId {
        ObjectId::from_bytes(&[byte; 20]).expect("oid")
    }

    fn create(name: &str) -> RefUpdate {
        RefUpdate {
            name: RefName::from_trusted(name.to_owned()),
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
    fn ignorecase_same_batch_case_variants_conflict() {
        let existing: BTreeMap<String, RawRef> = BTreeMap::new();
        let err = verify_create_casefold_conflicts(
            true,
            &existing,
            &[create("refs/heads/Foo"), create("refs/heads/foo")],
        )
        .expect_err("same-batch case pair");
        assert_eq!(
            err,
            RefStoreError::CaseConflict {
                name: "refs/heads/Foo".to_owned(),
                other: "refs/heads/foo".to_owned(),
            }
        );
        verify_create_casefold_against_existing(
            true,
            &existing,
            &[create("refs/heads/Foo"), create("refs/heads/foo")],
        )
        .expect("initial packed clone allows both spellings");
    }

    #[test]
    fn ascii_casefold_ref_paths_collide_detects_nested_parallel_trees() {
        assert!(ascii_casefold_ref_paths_collide(
            "refs/heads/Foo/bar",
            "refs/heads/foo/baz"
        ));
        assert!(ascii_casefold_ref_paths_collide(
            "refs/heads/foo/baz",
            "refs/heads/Foo/bar"
        ));
        assert!(!ascii_casefold_ref_paths_collide(
            "refs/heads/main",
            "refs/heads/other"
        ));
    }

    fn ignorecase_rejects_casefold_duplicate_create() {
        let existing: BTreeMap<String, RawRef> =
            [("refs/heads/Foo".to_owned(), RawRef::Direct(oid(2)))].into();
        let err = verify_create_casefold_conflicts(true, &existing, &[create("refs/heads/foo")])
            .expect_err("casefold collision");
        assert_eq!(
            err,
            RefStoreError::CreateExists {
                name: "refs/heads/foo".to_owned()
            }
        );
        verify_create_casefold_conflicts(false, &existing, &[create("refs/heads/foo")])
            .expect("case-sensitive allows distinct spellings");
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
