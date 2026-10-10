//! Reachability queries using pack / MIDX commit bitmaps (Git `prepare_bitmap_walk` semantics).
//!
//! Computes want/have sets with optional object filters, falling back to callers when a query
//! cannot be served from on-disk bitmaps alone.

use std::collections::{HashSet, VecDeque};

use crate::error::Error;
use crate::ewah_bitmap::Bitmap;
use crate::objects::{parse_commit, parse_tag, parse_tree, ObjectId, ObjectKind};
use crate::pack::read_object_info_from_packs;
use crate::pack_bitmap::{BitmapError, BitmapIndex};
use crate::repo::Repository;
use crate::rev_list::{shallow_boundary_oids, FilterObjectKind, MissingAction, ObjectFilter};

/// Query parameters for a bitmap reachability walk.
#[derive(Debug, Clone, Copy)]
pub struct ReachabilityQuery<'a> {
    /// Tips whose reachable objects are included in the result (after subtracting [`Self::haves`]).
    pub wants: &'a [ObjectId],
    /// Tips whose reachable objects are excluded from the result.
    pub haves: &'a [ObjectId],
    /// Optional object filter (see [`ObjectFilter`]); unsupported specs yield [`BitmapWalkError::Unsupported`].
    pub filter: Option<&'a ObjectFilter>,
}

/// Bitmap walk could not run (unsupported filter or shallow repo); use a non-bitmap walk instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("bitmap reachability walk is not supported for this repository or query")]
pub struct BitmapWalkUnsupported;

/// Errors during a bitmap reachability walk.
#[derive(Debug, thiserror::Error)]
pub enum BitmapWalkError {
    /// Filter or repository shape requires a non-bitmap walk.
    #[error(transparent)]
    Unsupported(#[from] BitmapWalkUnsupported),
    /// On-disk bitmap could not be read.
    #[error(transparent)]
    Bitmap(#[from] BitmapError),
    /// Repository or object access failure.
    #[error(transparent)]
    Repository(#[from] Error),
}

/// Reachable objects from a [`ReachabilityQuery`]: in-index bits plus out-of-namespace oids.
#[derive(Clone)]
pub struct ReachableSet {
    index: std::sync::Arc<BitmapIndex>,
    bits: Bitmap,
    extended: Vec<(ObjectId, ObjectKind)>,
    extended_set: HashSet<ObjectId>,
}

impl ReachableSet {
    /// Total number of reachable objects (indexed and extended).
    #[must_use]
    pub fn count(&self) -> usize {
        self.bits.count_ones() + self.extended.len()
    }

    /// Whether `oid` is in this set.
    #[must_use]
    pub fn contains(&self, oid: &ObjectId) -> bool {
        if self.extended_set.contains(oid) {
            return true;
        }
        self.index
            .position_of(oid)
            .is_some_and(|pos| self.bits.get(pos as usize))
    }

    /// Count reachable objects of `kind`.
    ///
    /// # Errors
    ///
    /// Propagates bitmap type-index decode failures.
    pub fn count_by_kind(&self, kind: ObjectKind) -> std::result::Result<usize, BitmapWalkError> {
        self.count_by_kind_popcount(kind)
    }

    /// Count reachable objects of `kind` via type-bitmap intersection popcount.
    ///
    /// # Errors
    ///
    /// Propagates bitmap type-index decode failures.
    pub fn count_by_kind_popcount(
        &self,
        kind: ObjectKind,
    ) -> std::result::Result<usize, BitmapWalkError> {
        let mut type_bits = Bitmap::new();
        self.index
            .type_bitmap(kind)
            .expand_into_bitmap(&mut type_bits)?;
        type_bits.and_assign(&self.bits);
        Ok(type_bits.count_ones() + self.extended.iter().filter(|(_, k)| *k == kind).count())
    }

    /// Iterate reachable oids of `kind` in ascending bitmap position order, then extended oids sorted.
    pub fn iter_by_kind(&self, kind: ObjectKind) -> impl Iterator<Item = ObjectId> + '_ {
        let positions = self.type_filtered_positions(kind);
        positions
            .into_iter()
            .filter_map(|pos| self.index.oid_at(pos).ok())
            .chain(
                self.extended
                    .iter()
                    .filter_map(move |(oid, k)| (*k == kind).then_some(*oid)),
            )
    }

    /// All reachable object ids (indexed namespace and extended) as a set.
    #[must_use]
    pub fn object_ids(&self) -> HashSet<ObjectId> {
        let mut out = HashSet::new();
        for pos in self.bits.set_bits() {
            if let Ok(oid) = self.index.oid_at(u32::try_from(pos).unwrap_or(u32::MAX)) {
                out.insert(oid);
            }
        }
        for (oid, _) in &self.extended {
            out.insert(*oid);
        }
        out
    }

    /// All reachable oids grouped by Git's bitmap output order (commits, trees, blobs, tags).
    pub fn iter_grouped_by_kind(&self) -> impl Iterator<Item = ObjectId> + '_ {
        [
            ObjectKind::Commit,
            ObjectKind::Tree,
            ObjectKind::Blob,
            ObjectKind::Tag,
        ]
        .into_iter()
        .flat_map(|kind| self.iter_by_kind(kind))
    }

    fn type_filtered_positions(&self, kind: ObjectKind) -> Vec<u32> {
        let Ok(mut type_bits) = (|| {
            let mut tb = Bitmap::new();
            self.index.type_bitmap(kind).expand_into_bitmap(&mut tb)?;
            Ok::<_, BitmapError>(tb)
        })() else {
            return Vec::new();
        };
        type_bits.and_assign(&self.bits);
        type_bits
            .set_bits()
            .map(|p| u32::try_from(p).unwrap_or(u32::MAX))
            .collect()
    }

    fn subtract(&mut self, other: &Self) {
        self.bits.and_not_assign(&other.bits);
        self.extended.retain(|(oid, _)| !other.contains(oid));
        self.extended_set
            .retain(|oid| !other.extended_set.contains(oid));
    }
}

/// Returns `true` when `filter` can be applied via type bitmaps (mirrors Git `can_filter_bitmap`).
#[must_use]
pub fn bitmap_filter_supported(filter: Option<&ObjectFilter>) -> bool {
    match filter {
        None => true,
        Some(ObjectFilter::SparseOid(_)) => false,
        Some(ObjectFilter::TreeDepth(depth)) if *depth > 0 => false,
        Some(ObjectFilter::Combine(parts)) => {
            parts.iter().all(|p| bitmap_filter_supported(Some(p)))
        }
        Some(ObjectFilter::BlobNone)
        | Some(ObjectFilter::BlobLimit(_))
        | Some(ObjectFilter::TreeDepth(0))
        | Some(ObjectFilter::ObjectType(_)) => true,
        Some(ObjectFilter::TreeDepth(_)) => false,
    }
}

/// Commit, tag, and loose object tips after ref peeling (avoids re-reading objects in bitmap walks).
pub(crate) struct PeeledTips {
    pub commits: Vec<ObjectId>,
    pub tags: Vec<ObjectId>,
    pub extra_objects: Vec<ObjectId>,
}

fn peel_tips(
    repo: &Repository,
    tips: &[ObjectId],
    missing: MissingAction,
) -> std::result::Result<PeeledTips, BitmapWalkError> {
    let mut out = PeeledTips {
        commits: Vec::new(),
        tags: Vec::new(),
        extra_objects: Vec::new(),
    };
    for &tip in tips {
        peel_one(repo, tip, missing, &mut out)?;
    }
    Ok(out)
}

fn peel_one(
    repo: &Repository,
    mut oid: ObjectId,
    missing: MissingAction,
    out: &mut PeeledTips,
) -> std::result::Result<(), BitmapWalkError> {
    loop {
        let object = match repo.odb.read(&oid) {
            Ok(obj) => obj,
            Err(Error::ObjectNotFound(_)) if missing != MissingAction::Error => return Ok(()),
            Err(err) => return Err(err.into()),
        };
        match object.kind {
            ObjectKind::Commit => {
                out.commits.push(oid);
                return Ok(());
            }
            ObjectKind::Tag => {
                out.tags.push(oid);
                let tag = parse_tag(&object.data)?;
                oid = tag.object;
            }
            ObjectKind::Tree | ObjectKind::Blob => {
                out.extra_objects.push(oid);
                return Ok(());
            }
        }
    }
}

impl BitmapIndex {
    /// Run `query` against this index and `repo`.
    ///
    /// # Errors
    ///
    /// Returns [`BitmapWalkError::Unsupported`] for shallow repositories or unsupported filters.
    pub fn reachability(
        self: &std::sync::Arc<Self>,
        repo: &Repository,
        query: ReachabilityQuery<'_>,
        missing: MissingAction,
    ) -> std::result::Result<ReachableSet, BitmapWalkError> {
        if !shallow_boundary_oids(&repo.git_dir).is_empty()
            || repo.git_dir.join("shallow").is_file()
        {
            return Err(BitmapWalkUnsupported.into());
        }
        if !bitmap_filter_supported(query.filter) {
            return Err(BitmapWalkUnsupported.into());
        }

        let haves = find_objects_union(repo, self, query.haves, missing, false)?;
        let mut wants = find_objects_union(repo, self, query.wants, missing, false)?;
        wants.subtract(&haves);
        apply_filter(repo, self, &mut wants, query.filter, query.wants)?;
        Ok(wants)
    }

    /// Compare the on-disk commit bitmap for `commit` with a fresh non-bitmap walk (`rev-list --test-bitmap`).
    ///
    /// Returns `Ok(true)` when they match, `Ok(false)` when the commit has no stored bitmap or the
    /// bitmap differs, and propagates read/decode errors.
    ///
    /// # Errors
    ///
    /// Propagates object read failures according to `missing`.
    pub fn verify_commit(
        self: &std::sync::Arc<Self>,
        repo: &Repository,
        commit: &ObjectId,
        missing: MissingAction,
    ) -> std::result::Result<bool, BitmapWalkError> {
        let Some(stored) = self.commit_bitmap(commit) else {
            return Ok(false);
        };
        let walked = find_objects_union(repo, self, &[*commit], missing, true)?;
        let stored_set: HashSet<u32> = stored.positions().collect();
        let walked_set: HashSet<u32> = walked
            .bits
            .set_bits()
            .map(|p| u32::try_from(p).unwrap_or(u32::MAX))
            .collect();
        Ok(stored_set == walked_set && walked.extended.is_empty())
    }

    /// Like [`Self::reachability`], but `wants` are already split by peeled kind (no tip re-read).
    pub(crate) fn reachability_with_peeled_wants(
        self: &std::sync::Arc<Self>,
        repo: &Repository,
        wants: PeeledTips,
        haves: &[ObjectId],
        filter: Option<&ObjectFilter>,
        missing: MissingAction,
        filter_want_tips: &[ObjectId],
    ) -> std::result::Result<ReachableSet, BitmapWalkError> {
        if !shallow_boundary_oids(&repo.git_dir).is_empty()
            || repo.git_dir.join("shallow").is_file()
        {
            return Err(BitmapWalkUnsupported.into());
        }
        if !bitmap_filter_supported(filter) {
            return Err(BitmapWalkUnsupported.into());
        }

        let haves = find_objects_union(repo, self, haves, missing, false)?;
        let mut want_set = find_objects_peeled(repo, self, wants, missing, false)?;
        want_set.subtract(&haves);
        apply_filter(repo, self, &mut want_set, filter, filter_want_tips)?;
        Ok(want_set)
    }
}

fn find_objects_union(
    repo: &Repository,
    index: &std::sync::Arc<BitmapIndex>,
    tips: &[ObjectId],
    missing: MissingAction,
    ignore_stored_commit_bitmaps: bool,
) -> std::result::Result<ReachableSet, BitmapWalkError> {
    if tips.is_empty() {
        return Ok(ReachableSet {
            index: std::sync::Arc::clone(index),
            bits: Bitmap::new(),
            extended: Vec::new(),
            extended_set: HashSet::new(),
        });
    }
    find_objects(repo, index, tips, missing, ignore_stored_commit_bitmaps)
}

fn find_objects(
    repo: &Repository,
    index: &std::sync::Arc<BitmapIndex>,
    tips: &[ObjectId],
    missing: MissingAction,
    ignore_stored_commit_bitmaps: bool,
) -> std::result::Result<ReachableSet, BitmapWalkError> {
    let peeled = peel_tips(repo, tips, missing)?;
    find_objects_peeled(repo, index, peeled, missing, ignore_stored_commit_bitmaps)
}

fn find_objects_peeled(
    repo: &Repository,
    index: &std::sync::Arc<BitmapIndex>,
    peeled: PeeledTips,
    missing: MissingAction,
    ignore_stored_commit_bitmaps: bool,
) -> std::result::Result<ReachableSet, BitmapWalkError> {
    let mut bits = Bitmap::new();
    let mut extended = Vec::new();
    let mut extended_set = HashSet::new();
    let mut pending_commits = VecDeque::new();

    for commit in peeled.commits {
        if try_or_commit_bitmap(index, &mut bits, &commit, ignore_stored_commit_bitmaps)? {
            continue;
        }
        if already_marked(index, &bits, &extended_set, &commit) {
            continue;
        }
        pending_commits.push_back(commit);
    }

    for tag in peeled.tags {
        mark_object(
            repo,
            index,
            &mut bits,
            &mut extended,
            &mut extended_set,
            tag,
            ObjectKind::Tag,
            missing,
        )?;
    }
    for oid in peeled.extra_objects {
        let kind = repo
            .odb
            .read_info(&oid)
            .map_err(BitmapWalkError::from)?
            .kind;
        mark_object(
            repo,
            index,
            &mut bits,
            &mut extended,
            &mut extended_set,
            oid,
            kind,
            missing,
        )?;
    }

    let mut seen_commits = HashSet::new();
    while let Some(commit) = pending_commits.pop_front() {
        if !seen_commits.insert(commit) {
            continue;
        }
        if try_or_commit_bitmap(index, &mut bits, &commit, ignore_stored_commit_bitmaps)? {
            if let Ok(parents) = read_commit(repo, commit, missing).map(|c| c.parents) {
                for parent in parents {
                    seen_commits.insert(parent);
                }
            }
            continue;
        }
        mark_index_or_extended(
            index,
            &mut bits,
            &mut extended,
            &mut extended_set,
            commit,
            ObjectKind::Commit,
        );
        let commit_data = match read_commit(repo, commit, missing) {
            Ok(c) => c,
            Err(Error::ObjectNotFound(_)) => continue,
            Err(err) => return Err(err.into()),
        };
        walk_tree(
            repo,
            index,
            &mut bits,
            &mut extended,
            &mut extended_set,
            commit_data.tree,
            missing,
        )?;
        for parent in commit_data.parents {
            if !seen_commits.contains(&parent) {
                pending_commits.push_back(parent);
            }
        }
    }

    Ok(ReachableSet {
        index: std::sync::Arc::clone(index),
        bits,
        extended,
        extended_set,
    })
}

fn try_or_commit_bitmap(
    index: &BitmapIndex,
    bits: &mut Bitmap,
    commit: &ObjectId,
    ignore_stored: bool,
) -> std::result::Result<bool, BitmapWalkError> {
    if ignore_stored {
        return Ok(false);
    }
    let Some(bm) = index.commit_bitmap(commit) else {
        return Ok(false);
    };
    bm.or_into(bits);
    Ok(true)
}

fn already_marked(
    index: &BitmapIndex,
    bits: &Bitmap,
    extended: &HashSet<ObjectId>,
    oid: &ObjectId,
) -> bool {
    if extended.contains(oid) {
        return true;
    }
    index
        .position_of(oid)
        .is_some_and(|pos| bits.get(pos as usize))
}

fn mark_index_or_extended(
    index: &BitmapIndex,
    bits: &mut Bitmap,
    extended: &mut Vec<(ObjectId, ObjectKind)>,
    extended_set: &mut HashSet<ObjectId>,
    oid: ObjectId,
    kind: ObjectKind,
) {
    if let Some(pos) = index.position_of(&oid) {
        bits.set(pos as usize);
    } else if extended_set.insert(oid) {
        extended.push((oid, kind));
    }
}

#[allow(clippy::too_many_arguments)]
fn mark_object(
    repo: &Repository,
    index: &BitmapIndex,
    bits: &mut Bitmap,
    extended: &mut Vec<(ObjectId, ObjectKind)>,
    extended_set: &mut HashSet<ObjectId>,
    oid: ObjectId,
    kind: ObjectKind,
    missing: MissingAction,
) -> std::result::Result<(), BitmapWalkError> {
    if already_marked(index, bits, extended_set, &oid) {
        return Ok(());
    }
    match kind {
        ObjectKind::Commit | ObjectKind::Tag | ObjectKind::Blob => {
            mark_index_or_extended(index, bits, extended, extended_set, oid, kind);
        }
        ObjectKind::Tree => {
            walk_tree(repo, index, bits, extended, extended_set, oid, missing)?;
        }
    }
    Ok(())
}

fn walk_tree(
    repo: &Repository,
    index: &BitmapIndex,
    bits: &mut Bitmap,
    extended: &mut Vec<(ObjectId, ObjectKind)>,
    extended_set: &mut HashSet<ObjectId>,
    tree: ObjectId,
    missing: MissingAction,
) -> std::result::Result<(), BitmapWalkError> {
    if already_marked(index, bits, extended_set, &tree) {
        return Ok(());
    }
    let object = match repo.odb.read(&tree) {
        Ok(obj) => obj,
        Err(Error::ObjectNotFound(_)) if missing != MissingAction::Error => return Ok(()),
        Err(err) => return Err(err.into()),
    };
    if object.kind != ObjectKind::Tree {
        return Ok(());
    }
    mark_index_or_extended(index, bits, extended, extended_set, tree, ObjectKind::Tree);
    let entries = parse_tree(&object.data)?;
    for entry in entries {
        // Submodule gitlinks name commits in another object store; do not walk them.
        if entry.mode == 0o160000 {
            continue;
        }
        if entry.mode == 0o040000 {
            walk_tree(
                repo,
                index,
                bits,
                extended,
                extended_set,
                entry.oid,
                missing,
            )?;
        } else {
            mark_index_or_extended(
                index,
                bits,
                extended,
                extended_set,
                entry.oid,
                ObjectKind::Blob,
            );
        }
    }
    Ok(())
}

fn read_commit(
    repo: &Repository,
    oid: ObjectId,
    missing: MissingAction,
) -> std::result::Result<crate::objects::CommitData, Error> {
    match repo.odb.read(&oid) {
        Ok(obj) if obj.kind == ObjectKind::Commit => parse_commit(&obj.data),
        Ok(_) => Err(Error::CorruptObject(format!("{oid} is not a commit"))),
        Err(Error::ObjectNotFound(_)) if missing != MissingAction::Error => {
            Err(Error::ObjectNotFound(oid.to_hex()))
        }
        Err(err) => Err(err),
    }
}

fn apply_filter(
    repo: &Repository,
    index: &BitmapIndex,
    set: &mut ReachableSet,
    filter: Option<&ObjectFilter>,
    want_tips: &[ObjectId],
) -> std::result::Result<(), BitmapWalkError> {
    let Some(filter) = filter else {
        return Ok(());
    };
    apply_filter_one(repo, index, set, filter, want_tips)?;
    Ok(())
}

fn apply_filter_one(
    repo: &Repository,
    index: &BitmapIndex,
    set: &mut ReachableSet,
    filter: &ObjectFilter,
    want_tips: &[ObjectId],
) -> std::result::Result<(), BitmapWalkError> {
    match filter {
        ObjectFilter::Combine(parts) => {
            for part in parts {
                apply_filter_one(repo, index, set, part, want_tips)?;
            }
        }
        ObjectFilter::BlobNone => {
            let keep = explicit_filter_roots(repo, index, want_tips)?.blobs;
            clear_type_except(index, &mut set.bits, ObjectKind::Blob, &keep);
            set.extended
                .retain(|(oid, kind)| *kind != ObjectKind::Blob || keep.contains(oid));
            rebuild_extended_set(set);
        }
        ObjectFilter::BlobLimit(limit) => {
            let tips = tip_blob_oids(repo, index, want_tips)?;
            filter_blob_limit(repo, index, &mut set.bits, &tips, *limit)?;
            set.extended.retain(|(oid, kind)| {
                if tips.contains(oid) {
                    return true;
                }
                *kind != ObjectKind::Blob || blob_under_limit(repo, oid, *limit)
            });
            rebuild_extended_set(set);
        }
        ObjectFilter::TreeDepth(0) => {
            let roots = explicit_filter_roots(repo, index, want_tips)?;
            clear_type_except(index, &mut set.bits, ObjectKind::Tree, &roots.trees);
            clear_type_except(index, &mut set.bits, ObjectKind::Blob, &roots.blobs);
            set.extended.retain(|(oid, kind)| {
                matches!(kind, ObjectKind::Commit | ObjectKind::Tag)
                    || roots.trees.contains(oid)
                    || roots.blobs.contains(oid)
            });
            rebuild_extended_set(set);
        }
        ObjectFilter::ObjectType(kind) => {
            keep_only_object_type(set, *kind);
        }
        ObjectFilter::SparseOid(_) | ObjectFilter::TreeDepth(_) => {
            return Err(BitmapWalkUnsupported.into());
        }
    }
    Ok(())
}

fn rebuild_extended_set(set: &mut ReachableSet) {
    set.extended_set = set.extended.iter().map(|(oid, _)| *oid).collect();
}

fn blob_under_limit(repo: &Repository, oid: &ObjectId, limit: u64) -> bool {
    match read_object_info_from_packs(repo.odb.objects_dir(), oid) {
        Ok(info) => info.size < limit,
        Err(_) => repo
            .odb
            .read_info(oid)
            .map(|info| info.size < limit)
            .unwrap_or(false),
    }
}

fn clear_type(index: &BitmapIndex, bits: &mut Bitmap, kind: ObjectKind) {
    clear_type_except(index, bits, kind, &HashSet::new());
}

fn clear_type_except(
    index: &BitmapIndex,
    bits: &mut Bitmap,
    kind: ObjectKind,
    keep: &HashSet<ObjectId>,
) {
    if let Ok(type_bits) = expand_type(index, kind) {
        for pos in type_bits.set_bits().collect::<Vec<_>>() {
            let pos_u32 = u32::try_from(pos).unwrap_or(u32::MAX);
            if let Ok(oid) = index.oid_at(pos_u32) {
                if keep.contains(&oid) {
                    continue;
                }
            }
            bits.clear(pos);
        }
    }
}

struct ExplicitFilterRoots {
    trees: HashSet<ObjectId>,
    blobs: HashSet<ObjectId>,
}

fn explicit_filter_roots(
    repo: &Repository,
    index: &BitmapIndex,
    tips: &[ObjectId],
) -> std::result::Result<ExplicitFilterRoots, BitmapWalkError> {
    let peeled = peel_tips(repo, tips, MissingAction::Error)?;
    let mut trees = HashSet::new();
    let mut blobs = HashSet::new();
    for oid in peeled.extra_objects {
        match repo
            .odb
            .read_info(&oid)
            .map_err(BitmapWalkError::from)?
            .kind
        {
            ObjectKind::Tree => {
                trees.insert(oid);
            }
            ObjectKind::Blob => {
                blobs.insert(oid);
            }
            ObjectKind::Commit | ObjectKind::Tag => {}
        }
    }
    for oid in tips {
        if let Some(pos) = index.position_of(oid) {
            if index
                .type_bitmap(ObjectKind::Tree)
                .contains(pos)
                .unwrap_or(false)
            {
                trees.insert(*oid);
            }
            if index
                .type_bitmap(ObjectKind::Blob)
                .contains(pos)
                .unwrap_or(false)
            {
                blobs.insert(*oid);
            }
        }
    }
    Ok(ExplicitFilterRoots { trees, blobs })
}

fn keep_only_object_type(set: &mut ReachableSet, kind: FilterObjectKind) {
    let object_kind = match kind {
        FilterObjectKind::Blob => ObjectKind::Blob,
        FilterObjectKind::Tree => ObjectKind::Tree,
        FilterObjectKind::Commit => ObjectKind::Commit,
        FilterObjectKind::Tag => ObjectKind::Tag,
    };
    for k in [
        ObjectKind::Commit,
        ObjectKind::Tree,
        ObjectKind::Blob,
        ObjectKind::Tag,
    ] {
        if k != object_kind {
            clear_type(&set.index, &mut set.bits, k);
        }
    }
    set.extended.retain(|(_, k)| *k == object_kind);
    rebuild_extended_set(set);
}

fn expand_type(
    index: &BitmapIndex,
    kind: ObjectKind,
) -> std::result::Result<Bitmap, BitmapWalkError> {
    let mut out = Bitmap::new();
    index
        .type_bitmap(kind)
        .expand_into_bitmap(&mut out)
        .map_err(BitmapWalkError::from)?;
    Ok(out)
}

fn tip_blob_oids(
    repo: &Repository,
    index: &BitmapIndex,
    tips: &[ObjectId],
) -> std::result::Result<HashSet<ObjectId>, BitmapWalkError> {
    Ok(explicit_filter_roots(repo, index, tips)?.blobs)
}

fn filter_blob_limit(
    repo: &Repository,
    index: &BitmapIndex,
    bits: &mut Bitmap,
    tip_blobs: &HashSet<ObjectId>,
    limit: u64,
) -> std::result::Result<(), BitmapWalkError> {
    let mut blob_type = expand_type(index, ObjectKind::Blob)?;
    blob_type.and_assign(bits);
    let objects_dir = repo.odb.objects_dir();
    for pos in blob_type.set_bits().collect::<Vec<_>>() {
        let pos_u32 = u32::try_from(pos).unwrap_or(u32::MAX);
        let Ok(oid) = index.oid_at(pos_u32) else {
            continue;
        };
        if tip_blobs.contains(&oid) {
            continue;
        }
        let size = read_object_info_from_packs(objects_dir, &oid)
            .map_err(BitmapWalkError::from)?
            .size;
        if size >= limit {
            bits.clear(pos);
        }
    }
    Ok(())
}
