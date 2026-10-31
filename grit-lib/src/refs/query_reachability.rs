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

/// One listed ref as the reachability filters see it.
#[derive(Debug, Clone, Copy)]
pub(super) struct ReachRow {
    /// Object the ref names.
    pub(super) oid: ObjectId,
    /// Annotated-tag peel: `Some(Some(target))` for a tag, `Some(None)` for a ref known not
    /// to name a tag, `None` while unknown. Resolving a row's commit tip fills it in, so the
    /// listing does not read the tip's header a second time to peel it.
    pub(super) peeled: Option<Option<ObjectId>>,
}

/// Commit `row` peels to, recording its tag peel when it was unknown.
fn peel_row_tip(
    repo: &Repository,
    row: &mut ReachRow,
) -> std::result::Result<Option<ObjectId>, RefQueryError> {
    let odb = repo.odb();
    if let Some(Some(p)) = row.peeled {
        if matches!(odb.read_info(&p), Ok(info) if info.kind == ObjectKind::Commit) {
            return Ok(Some(p));
        }
    }
    if row.peeled.is_none() {
        let (target, kind) = odb.peel_tags(row.oid).map_err(RefQueryError::Store)?;
        row.peeled = Some((target != row.oid).then_some(target));
        if kind == ObjectKind::Commit {
            return Ok(Some(target));
        }
    } else if let Some(commit) = odb.peel_to_commit(row.oid).map_err(RefQueryError::Store)? {
        return Ok(Some(commit));
    }
    Ok(peel_to_commit_for_merge_base(repo, row.oid).ok())
}

/// Resolves each distinct ref oid to its commit tip once.
///
/// Many refs often share a tip, and each resolution reads object headers, so rows pointing
/// at an already-resolved oid reuse its answer (tip and tag peel).
struct TipResolver {
    resolved: OidHashMap<(Option<ObjectId>, Option<ObjectId>)>,
}

impl TipResolver {
    fn new() -> Self {
        Self {
            resolved: oid_hash_map(),
        }
    }

    fn tip(
        &mut self,
        repo: &Repository,
        row: &mut ReachRow,
    ) -> std::result::Result<Option<ObjectId>, RefQueryError> {
        if let Some(&(tip, peeled)) = self.resolved.get(&row.oid) {
            row.peeled.get_or_insert(peeled);
            return Ok(tip);
        }
        let tip = peel_row_tip(repo, row)?;
        self.resolved
            .insert(row.oid, (tip, row.peeled.unwrap_or_default()));
        Ok(tip)
    }
}

struct ReachabilityFilterCtx<'a> {
    contains: &'a [ObjectId],
    no_contains: &'a [ObjectId],
    merged: &'a [ObjectId],
    no_merged: &'a [ObjectId],
    merged_tips: &'a OidHashSet,
    no_merged_tips: &'a OidHashSet,
}

/// `tip` is the commit `ref_oid` peels to ([`TipResolver::tip`]); `None` fails every commit
/// filter.
fn row_passes_reachability(
    query: &RefQuery,
    odb: &Odb,
    ref_oid: &ObjectId,
    tip: Option<ObjectId>,
    ctx: &ReachabilityFilterCtx<'_>,
    tip_idx: Option<usize>,
    batch: Option<&RefContainsBatch<'_>>,
) -> std::result::Result<bool, RefQueryError> {
    let needs_commit = !ctx.contains.is_empty()
        || !ctx.no_contains.is_empty()
        || !ctx.merged.is_empty()
        || !ctx.no_merged.is_empty();

    if !needs_commit {
        return ref_points_at_any(odb, *ref_oid, &query.points_at).map_err(RefQueryError::Store);
    }

    let Some(tip) = tip else {
        return Ok(false);
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

/// Rows are in name order, so result order is index order (reversed for a reverse sort).
fn filter_bounded_contains(
    repo: &Repository,
    query: &RefQuery,
    rows: &mut [ReachRow],
    mut evaluator: RefContainsMemo<'_>,
    limit: usize,
) -> std::result::Result<Vec<bool>, RefQueryError> {
    let len = rows.len();
    let mut keep = vec![false; len];
    let mut kept = 0usize;
    let mut tips = TipResolver::new();
    for step in 0..len {
        if kept == limit {
            break;
        }
        let idx = if query.sort.reverse {
            len - 1 - step
        } else {
            step
        };
        if let Some(tip) = tips.tip(repo, &mut rows[idx])? {
            if evaluator.contains(tip)? {
                keep[idx] = true;
                kept += 1;
            }
        }
    }
    Ok(keep)
}

/// Decide which `rows` pass the reachability filters in `query`.
///
/// `rows` must be in ref-name order. Returns one keep flag per row. Every row whose commit
/// tip the filters resolve gets its [`ReachRow::peeled`] filled in; rows the filters never
/// needed to peel keep whatever they came with.
pub(super) fn filter_listings_by_reachability(
    repo: &Repository,
    query: &RefQuery,
    rows: &mut [ReachRow],
) -> std::result::Result<Vec<bool>, RefQueryError> {
    let needs_commit = !query.contains.is_empty()
        || !query.no_contains.is_empty()
        || !query.merged.is_empty()
        || !query.no_merged.is_empty();
    if !needs_commit && query.points_at.is_empty() {
        return Ok(vec![true; rows.len()]);
    }

    let contains = resolve_targets(repo, &query.contains)?;
    let no_contains = resolve_targets(repo, &query.no_contains)?;
    let merged = resolve_targets(repo, &query.merged)?;
    let no_merged = resolve_targets(repo, &query.no_merged)?;
    if can_bound_contains(query) {
        if let Some(limit) = query.limit {
            if limit == 0 || rows.is_empty() {
                return Ok(vec![false; rows.len()]);
            }
            if let Some(evaluator) = RefContainsMemo::new(repo, &contains)? {
                return filter_bounded_contains(repo, query, rows, evaluator, limit);
            }
        }
    }
    let odb = repo.odb();

    let row_tips = if needs_commit {
        let mut tips = TipResolver::new();
        rows.iter_mut()
            .map(|row| tips.tip(repo, row))
            .collect::<std::result::Result<Vec<_>, _>>()?
    } else {
        vec![None; rows.len()]
    };

    let tips_for_merged: Vec<ObjectId> = if merged.is_empty() && no_merged.is_empty() {
        Vec::new()
    } else {
        let set: OidHashSet = row_tips.iter().flatten().copied().collect();
        set.into_iter().collect()
    };
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
    let mut batch = None;
    let row_tip_idx: Vec<Option<usize>> = if needs_contains {
        let batch = batch.insert(RefContainsBatch::new(repo));
        let idx = row_tips
            .iter()
            .map(|tip| tip.map(|t| batch.register_tip(t)))
            .collect();
        batch.run(&contains, &no_contains)?;
        idx
    } else {
        vec![None; rows.len()]
    };
    let batch_ref = batch.as_ref();

    rows.iter()
        .zip(row_tips)
        .zip(row_tip_idx)
        .map(|((row, tip), tip_idx)| {
            row_passes_reachability(query, odb, &row.oid, tip, &ctx, tip_idx, batch_ref)
        })
        .collect()
}
