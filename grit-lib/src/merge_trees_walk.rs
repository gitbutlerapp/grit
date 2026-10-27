//! Recursive three-tree merge: skip identical subtrees and compose the result tree in the ODB.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::merge_tree_cache::{
    find_tree_entry, lookup_index_entry_at_path_cached, side_tree_has_descendant_at_cached,
    MergeTreeCache, MergeTreeSide,
};
use crate::index::IndexEntry;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_one_path, merge_rename_rename, renamed_file_content, PathMergeContext,
    RenameRenameConflict, TreeMergeConflictPresentation, TreeMergeOutput, WhitespaceMergeOptions,
};
use crate::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::write_tree::is_empty_tree_oid;

const MODE_DIR: u32 = 0o040000;

/// Index over a set of paths answering "is anything at or under `prefix`" and
/// "is anything strictly under `prefix/`" with hash lookups instead of scans.
pub(crate) struct PathPrefixIndex<'a> {
    paths: HashSet<&'a [u8]>,
    ancestor_dirs: HashSet<&'a [u8]>,
}

impl<'a> PathPrefixIndex<'a> {
    pub(crate) fn new(paths: impl IntoIterator<Item = &'a Vec<u8>>) -> Self {
        let mut index = Self {
            paths: HashSet::new(),
            ancestor_dirs: HashSet::new(),
        };
        for path in paths {
            index.paths.insert(path.as_slice());
            for (i, &b) in path.iter().enumerate() {
                if b == b'/' && !index.ancestor_dirs.contains(&path[..i]) {
                    index.ancestor_dirs.insert(&path[..i]);
                }
            }
        }
        index
    }

    /// True when some path equals `prefix` or lies under `prefix/` (any path at all
    /// for the empty prefix).
    pub(crate) fn subtree_touched(&self, prefix: &[u8]) -> bool {
        if prefix.is_empty() {
            return !self.paths.is_empty();
        }
        self.paths.contains(prefix) || self.ancestor_dirs.contains(prefix)
    }

    /// True when some path lies strictly under `prefix/`.
    pub(crate) fn has_descendant(&self, prefix: &[u8]) -> bool {
        self.ancestor_dirs.contains(prefix)
    }

    /// True when `path` itself is in the set.
    pub(crate) fn contains(&self, path: &[u8]) -> bool {
        self.paths.contains(path)
    }
}

pub(crate) fn tree_entry_to_index(path: &[u8], te: &TreeEntry) -> IndexEntry {
    IndexEntry {
        ctime_sec: 0,
        ctime_nsec: 0,
        mtime_sec: 0,
        mtime_nsec: 0,
        dev: 0,
        ino: 0,
        mode: te.mode,
        uid: 0,
        gid: 0,
        size: 0,
        oid: te.oid,
        flags: path.len().min(0xFFF) as u16,
        flags_extended: None,
        path: path.to_vec(),
        base_index_pos: 0,
    }
}

pub(crate) fn lookup_index_entry_at_path(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    path: &[u8],
) -> crate::error::Result<Option<IndexEntry>> {
    lookup_index_entry_at_path_cached(cache, odb, tree_oid, path)
}

/// The file (blob, symlink or gitlink) at `path` in `tree_oid`.
///
/// A directory at `path` is not a file there: when one side turned a directory into a file, the
/// other side's directory must not take part in the three-way merge of that file.
fn file_entry_at_path(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    path: &[u8],
) -> crate::error::Result<Option<IndexEntry>> {
    Ok(lookup_index_entry_at_path(cache, odb, tree_oid, path)?.filter(|e| e.mode != MODE_DIR))
}

type SparseSideMaps = (
    BTreeMap<Vec<u8>, IndexEntry>,
    BTreeMap<Vec<u8>, IndexEntry>,
    BTreeMap<Vec<u8>, IndexEntry>,
);

pub(crate) fn build_sparse_maps(
    cache: &mut MergeTreeCache,
    repo: &Repository,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    paths: &HashSet<Vec<u8>>,
) -> crate::error::Result<SparseSideMaps> {
    let odb = repo.odb();
    let mut base = BTreeMap::new();
    let mut ours = BTreeMap::new();
    let mut theirs = BTreeMap::new();
    let empty_tree = odb.empty_tree_oid()?;
    let touched = PathPrefixIndex::new(paths);
    fill_sparse_maps_walk(
        cache,
        odb,
        b"",
        &base_tree,
        &ours_tree,
        &theirs_tree,
        &touched,
        &empty_tree,
        &mut base,
        &mut ours,
        &mut theirs,
    )?;
    Ok((base, ours, theirs))
}

#[expect(clippy::too_many_arguments)]
fn fill_sparse_maps_walk(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    prefix: &[u8],
    base_oid: &ObjectId,
    ours_oid: &ObjectId,
    theirs_oid: &ObjectId,
    touched: &PathPrefixIndex<'_>,
    empty_tree: &ObjectId,
    base: &mut BTreeMap<Vec<u8>, IndexEntry>,
    ours: &mut BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &mut BTreeMap<Vec<u8>, IndexEntry>,
) -> crate::error::Result<()> {
    if !prefix.is_empty() && !touched.subtree_touched(prefix) {
        return Ok(());
    }

    let base_entries = cache.tree_entries(odb, base_oid)?;
    let ours_entries = cache.tree_entries(odb, ours_oid)?;
    let theirs_entries = cache.tree_entries(odb, theirs_oid)?;

    let mut names: BTreeMap<Vec<u8>, ()> = BTreeMap::new();
    for e in base_entries.iter() {
        names.insert(e.name.clone(), ());
    }
    for e in ours_entries.iter() {
        names.insert(e.name.clone(), ());
    }
    for e in theirs_entries.iter() {
        names.insert(e.name.clone(), ());
    }

    for (name, ()) in names {
        let fp = full_path(prefix, &name);
        let b_te = find_tree_entry(&base_entries, &name);
        let o_te = find_tree_entry(&ours_entries, &name);
        let t_te = find_tree_entry(&theirs_entries, &name);

        if touched.contains(&fp) {
            if let Some(te) = b_te {
                if te.mode != MODE_DIR {
                    base.insert(fp.clone(), tree_entry_to_index(&fp, te));
                    cache.note_file_path(MergeTreeSide::Base, &fp);
                }
            }
            if let Some(te) = o_te {
                if te.mode != MODE_DIR {
                    ours.insert(fp.clone(), tree_entry_to_index(&fp, te));
                    cache.note_file_path(MergeTreeSide::Ours, &fp);
                }
            }
            if let Some(te) = t_te {
                if te.mode != MODE_DIR {
                    theirs.insert(fp.clone(), tree_entry_to_index(&fp, te));
                    cache.note_file_path(MergeTreeSide::Theirs, &fp);
                }
            }
        }

        let b_sub = b_te.filter(|e| e.mode == MODE_DIR).map(|e| e.oid);
        let o_sub = o_te.filter(|e| e.mode == MODE_DIR).map(|e| e.oid);
        let t_sub = t_te.filter(|e| e.mode == MODE_DIR).map(|e| e.oid);
        if !touched.subtree_touched(&fp) {
            continue;
        }

        fill_sparse_maps_walk(
            cache,
            odb,
            &fp,
            &b_sub.unwrap_or(*empty_tree),
            &o_sub.unwrap_or(*empty_tree),
            &t_sub.unwrap_or(*empty_tree),
            touched,
            empty_tree,
            base,
            ours,
            theirs,
        )?;
    }
    Ok(())
}

pub(crate) fn full_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut p = prefix.to_vec();
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }
}

pub(crate) fn side_tree_has_descendant_at(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    side: MergeTreeSide,
    prefix: &[u8],
    sparse_paths: &PathPrefixIndex<'_>,
) -> crate::error::Result<bool> {
    side_tree_has_descendant_at_cached(cache, odb, tree_oid, side, prefix, sparse_paths)
}

/// True when `rows` holds a path strictly under `dir/`.
fn has_rows_under(rows: &BTreeMap<&[u8], &IndexEntry>, dir: &[u8]) -> bool {
    let mut prefix = dir.to_vec();
    prefix.push(b'/');
    rows.range::<&[u8], _>((
        std::ops::Bound::Included(prefix.as_slice()),
        std::ops::Bound::Unbounded,
    ))
    .next()
    .is_some_and(|(path, _)| path.starts_with(&prefix))
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn compose_merged_tree_oid(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    prefix: &[u8],
    base_oid: &ObjectId,
    ours_oid: &ObjectId,
    theirs_oid: &ObjectId,
    touched: &PathPrefixIndex<'_>,
    renamed_away: &HashSet<Vec<u8>>,
    stage0_by_path: &BTreeMap<&[u8], &IndexEntry>,
    empty_tree: &ObjectId,
) -> crate::error::Result<ObjectId> {
    if !touched.subtree_touched(prefix) {
        return Ok(*ours_oid);
    }

    let base_entries = cache.tree_entries(odb, base_oid)?;
    let ours_entries = cache.tree_entries(odb, ours_oid)?;
    let theirs_entries = cache.tree_entries(odb, theirs_oid)?;

    let mut names: BTreeMap<Vec<u8>, ()> = BTreeMap::new();
    for e in base_entries.iter() {
        names.insert(e.name.clone(), ());
    }
    for e in ours_entries.iter() {
        names.insert(e.name.clone(), ());
    }
    for e in theirs_entries.iter() {
        names.insert(e.name.clone(), ());
    }
    let mut dir_prefix = prefix.to_vec();
    if !dir_prefix.is_empty() {
        dir_prefix.push(b'/');
    }
    // Keys sharing a byte prefix are contiguous in the ordered map.
    for path in stage0_by_path
        .range::<[u8], _>((
            std::ops::Bound::Included(dir_prefix.as_slice()),
            std::ops::Bound::Unbounded,
        ))
        .map(|(path, _)| path)
        .take_while(|path| path.starts_with(&dir_prefix))
    {
        let rel = &path[dir_prefix.len()..];
        if let Some(slash) = rel.iter().position(|&b| b == b'/') {
            names.insert(rel[..slash].to_vec(), ());
        } else if !rel.is_empty() {
            names.insert(rel.to_vec(), ());
        }
    }

    let mut out_entries: Vec<TreeEntry> = Vec::new();
    for (name, ()) in names {
        let fp = full_path(prefix, &name);
        let b_te = find_tree_entry(&base_entries, &name);
        let o_te = find_tree_entry(&ours_entries, &name);
        let t_te = find_tree_entry(&theirs_entries, &name);

        if !touched.subtree_touched(&fp) {
            if let Some(te) = o_te {
                if te.mode == MODE_DIR && is_empty_tree_oid(odb, &te.oid) {
                    continue;
                }
                out_entries.push(te.clone());
            }
            continue;
        }

        if let Some(entry) = stage0_by_path.get(fp.as_slice()) {
            out_entries.push(TreeEntry {
                mode: entry.mode,
                name,
                oid: entry.oid,
            });
            continue;
        }

        // Merged rows under `fp/` make it a directory, whatever ours has there: a side that
        // turned a file into a directory wins over a side that left the file alone.
        let mode = if has_rows_under(stage0_by_path, &fp) {
            MODE_DIR
        } else {
            o_te.or(t_te).or(b_te).map(|e| e.mode).unwrap_or(MODE_DIR)
        };
        if mode == MODE_DIR {
            let b_sub = b_te
                .filter(|e| e.mode == MODE_DIR)
                .map(|e| e.oid)
                .unwrap_or(*empty_tree);
            let o_sub = o_te
                .filter(|e| e.mode == MODE_DIR)
                .map(|e| e.oid)
                .unwrap_or(*empty_tree);
            let t_sub = t_te
                .filter(|e| e.mode == MODE_DIR)
                .map(|e| e.oid)
                .unwrap_or(*empty_tree);
            let sub = compose_merged_tree_oid(
                cache,
                odb,
                &fp,
                &b_sub,
                &o_sub,
                &t_sub,
                touched,
                renamed_away,
                stage0_by_path,
                empty_tree,
            )?;
            if is_empty_tree_oid(odb, &sub) {
                continue;
            }
            out_entries.push(TreeEntry {
                mode: MODE_DIR,
                name,
                oid: sub,
            });
            continue;
        }

        if let Some(te) = o_te {
            if renamed_away.contains(&fp) {
                continue;
            }
            // Unchanged blob under a touched directory (no sparse index entry).
            let keep = match t_te {
                None => !matches!(b_te, Some(b) if b.oid == te.oid && b.mode == te.mode),
                Some(t) => t.oid == te.oid && t.mode == te.mode,
            };
            if keep {
                out_entries.push(te.clone());
            }
        }
    }

    out_entries.sort_by(|a, b| {
        crate::objects::tree_entry_cmp(&a.name, a.mode == MODE_DIR, &b.name, b.mode == MODE_DIR)
    });

    let data = serialize_tree(&out_entries);
    odb.write(ObjectKind::Tree, &data)
}

/// The three trees of a merge with their sparse path → entry maps.
struct MergeSides<'a> {
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    base: &'a BTreeMap<Vec<u8>, IndexEntry>,
    ours: &'a BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &'a BTreeMap<Vec<u8>, IndexEntry>,
}

impl MergeSides<'_> {
    fn side(&self, side: MergeTreeSide) -> (&BTreeMap<Vec<u8>, IndexEntry>, &ObjectId) {
        match side {
            MergeTreeSide::Base => (self.base, &self.base_tree),
            MergeTreeSide::Ours => (self.ours, &self.ours_tree),
            MergeTreeSide::Theirs => (self.theirs, &self.theirs_tree),
        }
    }

    /// The file at `path` on `side`, from the sparse map or else the side's tree.
    fn file(
        &self,
        cache: &mut MergeTreeCache,
        odb: &Odb,
        side: MergeTreeSide,
        path: &[u8],
    ) -> Option<IndexEntry> {
        let (map, tree) = self.side(side);
        map.get(path)
            .cloned()
            .or_else(|| file_entry_at_path(cache, odb, tree, path).ok().flatten())
    }

    /// True when `side` has a file at `path`; [`Self::file`] without the copy.
    fn has_file(
        &self,
        cache: &mut MergeTreeCache,
        odb: &Odb,
        side: MergeTreeSide,
        path: &[u8],
    ) -> bool {
        let (map, tree) = self.side(side);
        map.contains_key(path)
            || file_entry_at_path(cache, odb, tree, path)
                .ok()
                .flatten()
                .is_some()
    }
}

/// A path absent from the base that both sides fill with a different file, at least one of
/// them by renaming a base file there (rename/rename 2to1 or rename/add).
struct RenameCollision<'a> {
    dest: &'a [u8],
    /// Base path ours renamed to `dest`; `None` when ours added `dest`.
    ours_source: Option<&'a [u8]>,
    /// Base path theirs renamed to `dest`; `None` when theirs added `dest`.
    theirs_source: Option<&'a [u8]>,
}

/// Rename destinations the other side also occupies, sorted by destination.
///
/// A source the other side renamed somewhere else as well is left to the path walk.
fn rename_collisions<'a>(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    sides: &MergeSides<'_>,
    ours_new_to_old: &'a HashMap<Vec<u8>, Vec<u8>>,
    ours_rename_sources: &HashSet<&[u8]>,
    theirs_old_to_new: &'a HashMap<Vec<u8>, Vec<u8>>,
    theirs_source_by_dest: &HashMap<&'a [u8], &'a [u8]>,
) -> Vec<RenameCollision<'a>> {
    let ours_source = |dest: &[u8]| {
        ours_new_to_old
            .get(dest)
            .map(Vec::as_slice)
            .filter(|old| *old != dest)
    };
    let theirs_source = |dest: &[u8]| {
        theirs_source_by_dest
            .get(dest)
            .copied()
            .filter(|old| *old != dest)
    };
    let renamed_elsewhere_by_theirs = |old: &[u8]| {
        theirs_old_to_new
            .get(old)
            .is_some_and(|new| new.as_slice() != old)
    };

    let mut dests: Vec<&'a [u8]> = ours_new_to_old
        .keys()
        .map(Vec::as_slice)
        .filter(|dest| ours_source(dest).is_some())
        .chain(
            theirs_source_by_dest
                .keys()
                .copied()
                .filter(|dest| theirs_source(dest).is_some()),
        )
        .collect();
    dests.sort_unstable();
    dests.dedup();

    let mut out = Vec::new();
    for dest in dests {
        let ours_source = ours_source(dest);
        let theirs_source = theirs_source(dest);
        if ours_source.is_some() && ours_source == theirs_source {
            continue;
        }
        if ours_source.is_some_and(renamed_elsewhere_by_theirs)
            || theirs_source.is_some_and(|old| ours_rename_sources.contains(old))
        {
            continue;
        }
        if !sides.has_file(cache, odb, MergeTreeSide::Ours, dest)
            || !sides.has_file(cache, odb, MergeTreeSide::Theirs, dest)
            || sides.has_file(cache, odb, MergeTreeSide::Base, dest)
        {
            continue;
        }
        out.push(RenameCollision {
            dest,
            ours_source,
            theirs_source,
        });
    }
    out
}

/// Record a [`RenameCollision`] the way Git does: an add/add conflict at the destination.
///
/// A side that renamed a file there contributes that file merged with the other side's copy
/// of the source (its own version when the other side deleted the source), so an edit made on
/// the other side is kept; the merged blob carries conflict markers when the edits overlap. A
/// side that added the destination contributes its blob. The two are then merged as an add/add.
#[expect(clippy::too_many_arguments)]
fn merge_rename_collision(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    ctx: &PathMergeContext<'_>,
    index: &mut crate::index::Index,
    conflict_content: &mut BTreeMap<Vec<u8>, ObjectId>,
    sides: &MergeSides<'_>,
    collision: &RenameCollision<'_>,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<()> {
    let dest = collision.dest;
    let (Some(mut at_ours), Some(mut at_theirs)) = (
        sides.file(cache, odb, MergeTreeSide::Ours, dest),
        sides.file(cache, odb, MergeTreeSide::Theirs, dest),
    ) else {
        return Ok(());
    };
    if let Some(source) = collision.ours_source {
        let base = sides.file(cache, odb, MergeTreeSide::Base, source);
        let other = sides.file(cache, odb, MergeTreeSide::Theirs, source);
        if let (Some(base), Some(other)) = (base, other) {
            if let Some(merged) = renamed_file_content(
                ctx,
                [source, dest, source],
                &base,
                &at_ours,
                &other,
                favor,
                ws,
                diff_algorithm,
                presentation,
            )? {
                at_ours = merged;
            }
        }
    }
    if let Some(source) = collision.theirs_source {
        let base = sides.file(cache, odb, MergeTreeSide::Base, source);
        let other = sides.file(cache, odb, MergeTreeSide::Ours, source);
        if let (Some(base), Some(other)) = (base, other) {
            if let Some(merged) = renamed_file_content(
                ctx,
                [source, source, dest],
                &base,
                &other,
                &at_theirs,
                favor,
                ws,
                diff_algorithm,
                presentation,
            )? {
                at_theirs = merged;
            }
        }
    }
    merge_one_path(
        ctx,
        index,
        conflict_content,
        dest,
        None,
        Some(&at_ours),
        Some(&at_theirs),
        favor,
        ws,
        diff_algorithm,
        presentation,
    )
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn three_way_on_touched_paths(
    cache: &mut MergeTreeCache,
    repo: &Repository,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    touched: &HashSet<Vec<u8>>,
    base: &BTreeMap<Vec<u8>, IndexEntry>,
    ours: &BTreeMap<Vec<u8>, IndexEntry>,
    theirs: &BTreeMap<Vec<u8>, IndexEntry>,
    ours_new_to_old: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
    theirs_old_to_new: &std::collections::HashMap<Vec<u8>, Vec<u8>>,
    rename_conflicts: Vec<RenameRenameConflict>,
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
    call_depth: usize,
) -> crate::error::Result<TreeMergeOutput> {
    use std::collections::HashSet as HSet;

    let odb = repo.odb();
    let ctx = PathMergeContext::new(repo, call_depth);
    let mut out = crate::index::Index::new();
    let mut conflict_content = BTreeMap::new();
    let mut handled_base: HSet<&[u8]> = HSet::new();
    let mut handled_ours: HSet<&[u8]> = HSet::new();
    let mut handled_theirs: HSet<&[u8]> = HSet::new();

    for conflict in &rename_conflicts {
        let (Some(b), Some(o), Some(t)) = (
            base.get(&conflict.base_path),
            ours.get(&conflict.ours_path),
            theirs.get(&conflict.theirs_path),
        ) else {
            continue;
        };
        merge_rename_rename(
            &ctx,
            &mut out,
            &mut conflict_content,
            conflict,
            b,
            o,
            t,
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
        handled_base.insert(conflict.base_path.as_slice());
        handled_theirs.insert(conflict.theirs_path.as_slice());
        handled_ours.extend([
            conflict.base_path.as_slice(),
            conflict.ours_path.as_slice(),
            conflict.theirs_path.as_slice(),
        ]);
    }

    let mut ours_paths: Vec<&[u8]> = touched.iter().map(Vec::as_slice).collect();
    for new in ours_new_to_old.keys() {
        ours_paths.push(new);
    }
    ours_paths.sort();
    ours_paths.dedup();

    let theirs_rename_source_by_dest: HashMap<&[u8], &[u8]> = theirs_old_to_new
        .iter()
        .map(|(old, new)| (new.as_slice(), old.as_slice()))
        .collect();

    // Base paths ours renamed away are merged at their ours destination.
    let ours_rename_sources: HSet<&[u8]> = ours_new_to_old
        .iter()
        .filter(|(new, old)| new != old)
        .map(|(_, old)| old.as_slice())
        .collect();

    let sides = MergeSides {
        base_tree,
        ours_tree,
        theirs_tree,
        base,
        ours,
        theirs,
    };
    for collision in rename_collisions(
        cache,
        odb,
        &sides,
        ours_new_to_old,
        &ours_rename_sources,
        theirs_old_to_new,
        &theirs_rename_source_by_dest,
    ) {
        merge_rename_collision(
            cache,
            odb,
            &ctx,
            &mut out,
            &mut conflict_content,
            &sides,
            &collision,
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
        let dest = collision.dest;
        handled_ours.insert(dest);
        handled_theirs.insert(dest);
        for source in [collision.ours_source, collision.theirs_source]
            .into_iter()
            .flatten()
        {
            handled_base.insert(source);
            handled_ours.insert(source);
            handled_theirs.insert(source);
        }
    }

    // When the other side renamed base paths to `new`, those destinations are also in
    // `touched`. When ours still has the rename source, the source iteration already merges
    // at `new`; skip duplicate passes (issue #1093). Do not skip when ours deleted the source.
    let mut theirs_rename_destinations: HSet<&[u8]> = HSet::new();
    for (old, new) in theirs_old_to_new {
        if old != new && touched.contains(old) {
            theirs_rename_destinations.insert(new.as_slice());
        }
    }

    for op in ours_paths {
        if handled_ours.contains(op) {
            continue;
        }
        if ours_rename_sources.contains(op)
            && !ours.contains_key(op)
            && lookup_index_entry_at_path(cache, odb, &ours_tree, op)?.is_none()
        {
            continue;
        }
        if theirs_rename_destinations.contains(op) && !ours_new_to_old.contains_key(op) {
            if let Some(old) = theirs_rename_source_by_dest.get(op) {
                let ours_still_at_source = sides.has_file(cache, odb, MergeTreeSide::Ours, old);
                if ours_still_at_source {
                    let ours_present_at_dest = sides.has_file(cache, odb, MergeTreeSide::Ours, op);
                    if !ours_present_at_dest {
                        if sides.has_file(cache, odb, MergeTreeSide::Theirs, op) {
                            handled_theirs.insert(op);
                        }
                        continue;
                    }
                }
            }
        }
        let bp = if let Some(old) = ours_new_to_old.get(op) {
            Some(old.as_slice())
        } else if base.contains_key(op) {
            Some(op)
        } else {
            None
        };

        let tp = bp.map_or(op, |path| {
            theirs_old_to_new.get(path).map_or(path, Vec::as_slice)
        });

        let b = bp.and_then(|p| sides.file(cache, odb, MergeTreeSide::Base, p));
        let o = sides.file(cache, odb, MergeTreeSide::Ours, op);
        let t = sides.file(cache, odb, MergeTreeSide::Theirs, tp);
        if t.is_some() {
            handled_theirs.insert(tp);
        }
        if let Some(p) = bp {
            handled_base.insert(p);
        }

        let out_path = if bp == Some(op) && tp != op { tp } else { op };

        merge_one_path(
            &ctx,
            &mut out,
            &mut conflict_content,
            out_path,
            b.as_ref(),
            o.as_ref(),
            t.as_ref(),
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    let mut base_paths: Vec<&[u8]> = touched.iter().map(Vec::as_slice).collect();
    base_paths.sort();
    base_paths.dedup();
    for bp in base_paths {
        if handled_base.contains(bp) {
            continue;
        }
        let tp = theirs_old_to_new.get(bp).map_or(bp, Vec::as_slice);
        let b = sides.file(cache, odb, MergeTreeSide::Base, bp);
        if b.is_none() {
            continue;
        }
        let t = sides.file(cache, odb, MergeTreeSide::Theirs, tp);
        if t.is_some() {
            handled_theirs.insert(tp);
        }
        merge_one_path(
            &ctx,
            &mut out,
            &mut conflict_content,
            bp,
            b.as_ref(),
            None,
            t.as_ref(),
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    let mut theirs_paths: Vec<&[u8]> = touched.iter().map(Vec::as_slice).collect();
    theirs_paths.sort();
    theirs_paths.dedup();
    for tp in theirs_paths {
        if handled_theirs.contains(tp) {
            continue;
        }
        let t = sides.file(cache, odb, MergeTreeSide::Theirs, tp);
        if t.is_none() {
            continue;
        }
        merge_one_path(
            &ctx,
            &mut out,
            &mut conflict_content,
            tp,
            None,
            None,
            t.as_ref(),
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    out.sort();

    Ok(TreeMergeOutput {
        index: out,
        conflict_content,
        result_tree: ObjectId::zero(),
        touched_paths: touched.clone(),
        rename_conflicts,
        directory_rename_conflicts: Vec::new(),
    })
}

pub(crate) fn finish_merge_result_tree(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    output: &TreeMergeOutput,
    base_tree: ObjectId,
    ours_tree: ObjectId,
    theirs_tree: ObjectId,
    renamed_away: &HashSet<Vec<u8>>,
) -> crate::error::Result<ObjectId> {
    let mut stage0 = BTreeMap::new();
    for entry in output.index.entries() {
        if entry.stage() == 0 {
            stage0.insert(entry.path.as_slice(), entry);
        }
    }
    for entry in output.index.entries() {
        if entry.stage() == 0 {
            continue;
        }
        if output.conflict_content.contains_key(&entry.path) {
            continue;
        }
        if (entry.stage() == 2 || entry.stage() == 3) && !stage0.contains_key(entry.path.as_slice())
        {
            stage0.insert(entry.path.as_slice(), entry);
        }
    }
    let empty_tree = odb.empty_tree_oid()?;
    let touched = PathPrefixIndex::new(&output.touched_paths);
    compose_merged_tree_oid(
        cache,
        odb,
        b"",
        &base_tree,
        &ours_tree,
        &theirs_tree,
        &touched,
        renamed_away,
        &stage0,
        &empty_tree,
    )
}

#[cfg(test)]
mod tests {
    use super::PathPrefixIndex;

    fn path_has_prefix(path: &[u8], prefix: &[u8]) -> bool {
        if prefix.is_empty() {
            return true;
        }
        path.starts_with(prefix) && (path.len() == prefix.len() || path[prefix.len()] == b'/')
    }

    fn naive_has_descendant(paths: &[Vec<u8>], prefix: &[u8]) -> bool {
        paths
            .iter()
            .any(|p| p.len() > prefix.len() && p.starts_with(prefix) && p[prefix.len()] == b'/')
    }

    #[test]
    fn path_prefix_index_matches_linear_scans() {
        let paths: Vec<Vec<u8>> = [
            "a",
            "ab/c",
            "d/e/f.txt",
            "d/e/g.txt",
            "d/h",
            "x//y",
            "deep/one/two/three/four",
        ]
        .iter()
        .map(|p| p.as_bytes().to_vec())
        .collect();
        let index = PathPrefixIndex::new(&paths);
        let probes = [
            "",
            "a",
            "a/",
            "ab",
            "ab/c",
            "ab/c/d",
            "abc",
            "d",
            "d/",
            "d/e",
            "d/e/f.txt",
            "d/e/f",
            "d/h",
            "d/h/i",
            "e",
            "x",
            "x/",
            "x//y",
            "deep",
            "deep/one",
            "deep/one/two/three",
            "deep/one/two/three/four",
            "deep/on",
        ];
        for probe in probes {
            let probe = probe.as_bytes();
            assert_eq!(
                index.subtree_touched(probe),
                paths.iter().any(|p| path_has_prefix(p, probe)),
                "subtree_touched({:?})",
                String::from_utf8_lossy(probe)
            );
            assert_eq!(
                index.has_descendant(probe),
                naive_has_descendant(&paths, probe),
                "has_descendant({:?})",
                String::from_utf8_lossy(probe)
            );
            assert_eq!(index.contains(probe), paths.iter().any(|p| p == probe));
        }
    }

    #[test]
    fn empty_path_prefix_index_reports_nothing_touched() {
        let index = PathPrefixIndex::new(std::iter::empty());
        assert!(!index.subtree_touched(b""));
        assert!(!index.subtree_touched(b"a"));
        assert!(!index.has_descendant(b""));
    }
}
