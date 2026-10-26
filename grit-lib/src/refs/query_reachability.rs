//! Reachability filters for [`super::listing::RefQuery`] (`--contains`, `--merged`, …).

use std::collections::HashSet;

use crate::error::Error;
use crate::merge_base::{branch_tips_merged_into, RefContainsBatch, RefContainsMemo};
use crate::objects::{oid_hash_map, oid_hash_set, OidHashMap, OidHashSet};
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
/// Propagates object read and parse failures (except missing intermediate objects, which
/// return `false`). Returns [`Error::CorruptObject`] when a tag peel cycle is detected.
pub fn ref_points_at(
    odb: &Odb,
    ref_oid: ObjectId,
    target: &ObjectId,
) -> crate::error::Result<bool> {
    if &ref_oid == target {
        return Ok(true);
    }
    let mut current = ref_oid;
    let mut seen = HashSet::new();
    loop {
        if !seen.insert(current) {
            return Err(Error::CorruptObject(format!(
                "annotated tag peel cycle at {current}"
            )));
        }
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
) -> std::result::Result<Option<ObjectId>, RefQueryError> {
    if let Some(p) = peeled {
        if matches!(
            repo.odb().read_info(&p),
            Ok(info) if info.kind == ObjectKind::Commit
        ) {
            return Ok(Some(p));
        }
    }
    match repo.odb().peel_to_commit(ref_oid) {
        Ok(opt) => {
            if opt.is_some() {
                return Ok(opt);
            }
        }
        Err(e) => return Err(RefQueryError::Store(e)),
    }
    Ok(peel_to_commit_for_merge_base(repo, ref_oid).ok())
}

struct ReachabilityFilterCtx<'a> {
    contains: &'a [ObjectId],
    no_contains: &'a [ObjectId],
    merged: &'a [ObjectId],
    no_merged: &'a [ObjectId],
    merged_tips: &'a OidHashSet,
    no_merged_tips: &'a OidHashSet,
}

fn row_passes_reachability<'repo>(
    repo: &'repo Repository,
    query: &RefQuery,
    odb: &Odb,
    row: &(String, ObjectId, Option<ObjectId>),
    ctx: &ReachabilityFilterCtx<'_>,
    tip_idx: Option<usize>,
    batch: Option<&RefContainsBatch<'repo>>,
) -> std::result::Result<bool, RefQueryError> {
    let (_, ref_oid, peeled) = row;
    let needs_commit = !ctx.contains.is_empty()
        || !ctx.no_contains.is_empty()
        || !ctx.merged.is_empty()
        || !ctx.no_merged.is_empty();

    if !needs_commit {
        return ref_points_at_any(odb, *ref_oid, &query.points_at).map_err(RefQueryError::Store);
    }

    let tip = match (tip_idx, batch) {
        (Some(idx), Some(batch)) => batch.tip_commit(idx),
        _ => match peel_ref_tip_commit(repo, *ref_oid, *peeled)? {
            Some(t) => t,
            None => return Ok(false),
        },
    };

    if !ctx.contains.is_empty() {
        let Some(idx) = tip_idx else {
            return Ok(false);
        };
        let Some(batch) = batch else {
            return Ok(false);
        };
        if !batch.tip_state(idx).contains_match {
            return Ok(false);
        }
    }
    if !ctx.no_contains.is_empty() {
        let Some(idx) = tip_idx else {
            return Ok(false);
        };
        let Some(batch) = batch else {
            return Ok(false);
        };
        if batch.tip_state(idx).no_contains_match {
            return Ok(false);
        }
    }

    if !ctx.merged.is_empty() && !ctx.merged_tips.contains(&tip) {
        return Ok(false);
    }
    if !ctx.no_merged.is_empty() && ctx.no_merged_tips.contains(&tip) {
        return Ok(false);
    }

    if !query.points_at.is_empty()
        && !ref_points_at_any(odb, *ref_oid, &query.points_at).map_err(RefQueryError::Store)?
    {
        return Ok(false);
    }

    Ok(true)
}

/// Name ordering can be decided before containment, so a finite query needs only
/// enough matching tips. Other filters and global sorts retain the batch walk.
pub(super) fn can_bound_contains(query: &RefQuery) -> bool {
    query.limit.is_some()
        && query.sort.key == super::listing::SortKey::Name
        && !query.contains.is_empty()
        && query.no_contains.is_empty()
        && query.merged.is_empty()
        && query.no_merged.is_empty()
        && query.points_at.is_empty()
}

fn filter_bounded_contains(
    repo: &Repository,
    query: &RefQuery,
    rows: &mut Vec<(String, ObjectId, Option<ObjectId>)>,
    targets: &[ObjectId],
    limit: usize,
) -> std::result::Result<(), RefQueryError> {
    if limit == 0 || rows.is_empty() {
        rows.clear();
        return Ok(());
    }
    let mut evaluator = RefContainsMemo::new(repo, targets)?;
    let mut peels = oid_hash_map();
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    if query.sort.reverse {
        rows.reverse();
    }
    let mut kept = Vec::with_capacity(limit.min(rows.len()));
    for row in rows.drain(..) {
        if kept.len() == limit {
            break;
        }
        let tip = match peels.get(&row.1) {
            Some(tip) => *tip,
            None => {
                let tip = peel_ref_tip_commit(repo, row.1, row.2)?;
                peels.insert(row.1, tip);
                tip
            }
        };
        if let Some(tip) = tip {
            if evaluator.contains(tip)? {
                kept.push(row);
            }
        }
    }
    *rows = kept;
    Ok(())
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
    if can_bound_contains(query) {
        if let Some(limit) = query.limit {
            return filter_bounded_contains(repo, query, rows, &contains, limit);
        }
    }
    let odb = repo.odb();

    let mut peel_cache: OidHashMap<ObjectId> = oid_hash_map();
    for (_, oid, peeled) in rows.iter_mut() {
        if peeled.is_some() {
            continue;
        }
        if let Some(&commit) = peel_cache.get(oid) {
            *peeled = Some(commit);
            continue;
        }
        if let Ok(Some(commit)) = peel_ref_tip_commit(repo, *oid, None) {
            peel_cache.insert(*oid, commit);
            *peeled = Some(commit);
        }
    }

    let mut tips_for_merged_set = oid_hash_set();
    if !merged.is_empty() || !no_merged.is_empty() {
        for (_, oid, peeled) in rows.iter() {
            if let Some(t) = peel_ref_tip_commit(repo, *oid, *peeled)? {
                tips_for_merged_set.insert(t);
            }
        }
    }
    let tips_for_merged: Vec<ObjectId> = tips_for_merged_set.into_iter().collect();
    let empty_merged = oid_hash_set();
    let merged_owned = if merged.is_empty() {
        None
    } else {
        Some(
            branch_tips_merged_into(repo, &merged, &tips_for_merged)
                .map_err(RefQueryError::Store)?,
        )
    };
    let no_merged_owned = if no_merged.is_empty() {
        None
    } else {
        Some(
            branch_tips_merged_into(repo, &no_merged, &tips_for_merged)
                .map_err(RefQueryError::Store)?,
        )
    };
    let merged_tips = merged_owned.as_ref().unwrap_or(&empty_merged);
    let no_merged_tips = no_merged_owned.as_ref().unwrap_or(&empty_merged);
    let ctx = ReachabilityFilterCtx {
        contains: &contains,
        no_contains: &no_contains,
        merged: &merged,
        no_merged: &no_merged,
        merged_tips,
        no_merged_tips,
    };

    let needs_contains = !contains.is_empty() || !no_contains.is_empty();
    let mut batch = RefContainsBatch::new(repo);
    let mut row_tip_idx: Vec<Option<usize>> = Vec::with_capacity(rows.len());
    if needs_contains {
        for (_, oid, peeled) in rows.iter() {
            row_tip_idx.push(
                peel_ref_tip_commit(repo, *oid, *peeled)?.map(|t| batch.register_tip(repo, t)),
            );
        }
        batch.run(&contains, &no_contains)?;
    } else {
        row_tip_idx = vec![None; rows.len()];
    }
    let batch_ref = needs_contains.then_some(&batch);

    let mut kept = Vec::with_capacity(rows.len());
    for (row, tip_idx) in rows.drain(..).zip(row_tip_idx.drain(..)) {
        if row_passes_reachability(repo, query, odb, &row, &ctx, tip_idx, batch_ref)? {
            kept.push(row);
        }
    }
    *rows = kept;
    Ok(())
}
