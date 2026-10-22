//! Reachability filters for [`super::listing::RefQuery`] (`--contains`, `--merged`, …).

use crate::error::Error;
use crate::merge_base::{branch_tips_merged_into, commit_tip_contains_all, RefContainsCache};
use crate::objects::{parse_tag, ObjectId, ObjectKind};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::rev_parse::peel_to_commit_for_merge_base;

use super::listing::{RefQuery, RefQueryError};

/// Resolve a `--contains` / `--merged` object id to a commit.
///
/// # Errors
///
/// Returns [`RefQueryError::MissingObject`] when the object is absent and
/// [`RefQueryError::NotCommit`] when it does not peel to a commit.
pub fn resolve_commit_filter_target(
    repo: &Repository,
    oid: ObjectId,
) -> std::result::Result<ObjectId, RefQueryError> {
    match repo.odb().read_info(&oid) {
        Err(Error::ObjectNotFound(_)) => Err(RefQueryError::MissingObject(oid)),
        Err(e) => Err(RefQueryError::Store(e)),
        Ok(info) => match info.kind {
            ObjectKind::Commit => Ok(oid),
            ObjectKind::Tag | ObjectKind::Tree | ObjectKind::Blob => {
                peel_to_commit_for_merge_base(repo, oid).map_err(|_| RefQueryError::NotCommit(oid))
            }
        },
    }
}

/// Whether `ref_oid` equals `target` or peels to it through annotated tags.
///
/// # Errors
///
/// Propagates object read and parse failures (except missing objects, which return `false`).
pub fn ref_points_at(
    odb: &Odb,
    ref_oid: ObjectId,
    target: &ObjectId,
) -> crate::error::Result<bool> {
    if &ref_oid == target {
        return Ok(true);
    }
    let mut current = ref_oid;
    for _ in 0..32 {
        let obj = match odb.read(&current) {
            Ok(o) => o,
            Err(Error::ObjectNotFound(_)) => return Ok(false),
            Err(e) => return Err(e),
        };
        if obj.kind != ObjectKind::Tag {
            return Ok(false);
        }
        let tag = parse_tag(&obj.data)?;
        if &tag.object == target {
            return Ok(true);
        }
        current = tag.object;
    }
    Ok(false)
}

/// Whether `ref_oid` matches any oid in `points_at` ([`ref_points_at`]).
pub fn ref_points_at_any(
    odb: &Odb,
    ref_oid: ObjectId,
    points_at: &[ObjectId],
) -> crate::error::Result<bool> {
    for target in points_at {
        if ref_points_at(odb, ref_oid, target)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn resolve_targets(
    repo: &Repository,
    oids: &[ObjectId],
) -> std::result::Result<Vec<ObjectId>, RefQueryError> {
    oids.iter()
        .map(|&oid| resolve_commit_filter_target(repo, oid))
        .collect()
}

fn peel_ref_tip_commit(
    repo: &Repository,
    ref_oid: ObjectId,
    peeled: Option<ObjectId>,
) -> Option<ObjectId> {
    if let Some(p) = peeled {
        return Some(p);
    }
    repo.odb()
        .peel_to_commit(ref_oid)
        .ok()
        .flatten()
        .or_else(|| peel_to_commit_for_merge_base(repo, ref_oid).ok())
}

/// Drop refs that fail reachability filters in `query`.
pub(super) fn filter_listings_by_reachability(
    repo: &Repository,
    query: &RefQuery,
    rows: &mut Vec<(String, ObjectId, Option<ObjectId>)>,
) -> std::result::Result<(), RefQueryError> {
    let needs_commit = !query.contains.is_empty()
        || !query.no_contains.is_empty()
        || !query.merged.is_empty()
        || !query.no_merged.is_empty();
    if !needs_commit && query.points_at.is_empty() {
        return Ok(());
    }

    let contains = resolve_targets(repo, &query.contains)?;
    let no_contains = resolve_targets(repo, &query.no_contains)?;
    let merged = resolve_targets(repo, &query.merged)?;
    let no_merged = resolve_targets(repo, &query.no_merged)?;
    let odb = repo.odb();

    if needs_commit {
        rows.retain(|(_, oid, peeled)| peel_ref_tip_commit(repo, *oid, *peeled).is_some());
    }

    if !contains.is_empty() || !no_contains.is_empty() {
        let mut cache = RefContainsCache::default();
        rows.retain(|(_, oid, peeled)| {
            let Some(tip) = peel_ref_tip_commit(repo, *oid, *peeled) else {
                return false;
            };
            commit_tip_contains_all(repo, tip, &contains, &mut cache).unwrap_or(false)
        });
        rows.retain(|(_, oid, peeled)| {
            let Some(tip) = peel_ref_tip_commit(repo, *oid, *peeled) else {
                return false;
            };
            for &target in &no_contains {
                if commit_tip_contains_all(repo, tip, &[target], &mut cache).unwrap_or(false) {
                    return false;
                }
            }
            true
        });
    }

    if !merged.is_empty() || !no_merged.is_empty() {
        let tips: Vec<ObjectId> = rows
            .iter()
            .filter_map(|(_, oid, peeled)| peel_ref_tip_commit(repo, *oid, *peeled))
            .collect();
        let merged_tips =
            branch_tips_merged_into(repo, &merged, &tips).map_err(RefQueryError::Store)?;
        let no_merged_tips =
            branch_tips_merged_into(repo, &no_merged, &tips).map_err(RefQueryError::Store)?;

        rows.retain(|(_, oid, peeled)| {
            let Some(tip) = peel_ref_tip_commit(repo, *oid, *peeled) else {
                return false;
            };
            if !merged.is_empty() && !merged_tips.contains(&tip) {
                return false;
            }
            if !no_merged.is_empty() && no_merged_tips.contains(&tip) {
                return false;
            }
            true
        });
    }

    if !query.points_at.is_empty() {
        rows.retain(|(_, oid, _)| ref_points_at_any(odb, *oid, &query.points_at).unwrap_or(false));
    }

    Ok(())
}
