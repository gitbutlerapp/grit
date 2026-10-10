//! Directory/file refname conflict checks for in-memory stores.

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use crate::refs::RefnameUnavailable;

use super::RawRef;
use super::RefUpdate;

fn refname_is_strict_prefix(parent: &str, child: &str) -> bool {
    child.len() > parent.len()
        && child.as_bytes().get(parent.len()) == Some(&b'/')
        && child.starts_with(parent)
}

/// Verify batch-internal D/F conflicts (Git `refs_verify_refnames_available` between extras).
pub fn verify_same_batch_conflicts(updates: &[RefUpdate]) -> Result<(), RefnameUnavailable> {
    let names: Vec<&str> = updates.iter().map(|u| u.name.as_str()).collect();
    for i in 0..names.len() {
        for j in (i + 1)..names.len() {
            let (a, b) = (names[i], names[j]);
            let (parent, child) = if refname_is_strict_prefix(a, b) {
                (a, b)
            } else if refname_is_strict_prefix(b, a) {
                (b, a)
            } else {
                continue;
            };
            return Err(RefnameUnavailable::SameBatch {
                refname: child.to_owned(),
                other: parent.to_owned(),
            });
        }
    }
    Ok(())
}

fn find_descendant_in_sorted(dirname_with_slash: &str, names: &BTreeSet<String>) -> Option<String> {
    let start = names.range(dirname_with_slash.to_string()..).next()?;
    if start.starts_with(dirname_with_slash) {
        Some(start.clone())
    } else {
        None
    }
}

/// Verify that each *new* ref in `updates` can be created without conflicting with `existing`
/// or other batch items. Deletes in the batch do not suppress conflicts (Git t1404).
pub fn verify_create_conflicts(
    existing: &BTreeMap<String, RawRef>,
    updates: &[RefUpdate],
) -> Result<(), RefnameUnavailable> {
    verify_same_batch_conflicts(updates)?;

    let extras: BTreeSet<String> = updates.iter().map(|u| u.name.clone()).collect();
    let existing_names: BTreeSet<String> = existing.keys().cloned().collect();

    for update in updates {
        if update.new_value.is_none() || update.flags.log_only {
            continue;
        }
        if existing.contains_key(&update.name) {
            continue;
        }
        verify_name_available_for_create(&update.name, &existing_names, &extras)?;
    }
    Ok(())
}

fn verify_name_available_for_create(
    refname: &str,
    existing: &BTreeSet<String>,
    extras: &BTreeSet<String>,
) -> Result<(), RefnameUnavailable> {
    let segments: Vec<&str> = refname.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() > 1 {
        let mut dirname = String::new();
        for part in &segments[..segments.len() - 1] {
            if !dirname.is_empty() {
                dirname.push('/');
            }
            dirname.push_str(part);
            if existing.contains(&dirname) {
                return Err(RefnameUnavailable::AncestorExists {
                    blocking: dirname.clone(),
                    new_ref: refname.to_owned(),
                });
            }
            if extras.contains(&dirname) {
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
            blocking,
            new_ref: refname.to_owned(),
        });
    }
    if let Some(blocking) = find_descendant_in_sorted(&leaf_dir, extras) {
        return Err(RefnameUnavailable::SameBatch {
            refname: refname.to_owned(),
            other: blocking,
        });
    }

    Ok(())
}
