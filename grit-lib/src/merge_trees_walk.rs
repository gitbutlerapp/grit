//! Recursive three-tree merge: skip identical subtrees and compose the result tree in the ODB.

use std::collections::{BTreeMap, HashMap, HashSet};

use super::merge_tree_cache::{
    find_tree_entry, lookup_index_entry_at_path_cached, side_tree_has_descendant_at_cached,
    MergeTreeCache, MergeTreeSide,
};
use crate::index::IndexEntry;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_one_path, PathMergeContext, TreeMergeConflictPresentation, TreeMergeOutput,
    WhitespaceMergeOptions,
};
use crate::objects::{serialize_tree, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;
use crate::repo::Repository;
use crate::write_tree::is_empty_tree_oid;

const MODE_DIR: u32 = 0o040000;

/// Index over a set of paths answering "is anything at or under `prefix`" and
/// "is anything strictly under `prefix/`" with hash lookups instead of scans.
pub(crate) struct PathPrefixIndex {
    paths: HashSet<Vec<u8>>,
    ancestor_dirs: HashSet<Vec<u8>>,
}

impl PathPrefixIndex {
    pub(crate) fn new<'a>(paths: impl IntoIterator<Item = &'a Vec<u8>>) -> Self {
        let mut index = Self {
            paths: HashSet::new(),
            ancestor_dirs: HashSet::new(),
        };
        for path in paths {
            index.paths.insert(path.clone());
            for (i, &b) in path.iter().enumerate() {
                if b == b'/' && !index.ancestor_dirs.contains(&path[..i]) {
                    index.ancestor_dirs.insert(path[..i].to_vec());
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

pub(crate) fn paths_from_tree_diff(
    odb: &Odb,
    base: &ObjectId,
    other: &ObjectId,
) -> crate::error::Result<HashSet<Vec<u8>>> {
    use crate::diff::{diff_trees, DiffStatus};
    let entries = diff_trees(odb, Some(base), Some(other), "")?;
    let mut out = HashSet::new();
    for e in entries {
        if e.status == DiffStatus::Unmerged {
            continue;
        }
        if let Some(p) = e.old_path {
            out.insert(p.into_bytes());
        }
        if let Some(p) = e.new_path {
            out.insert(p.into_bytes());
        }
    }
    Ok(out)
}

pub(crate) fn collect_touched_merge_paths(
    odb: &Odb,
    base_tree: &ObjectId,
    ours_tree: &ObjectId,
    theirs_tree: &ObjectId,
) -> crate::error::Result<HashSet<Vec<u8>>> {
    let mut touched = paths_from_tree_diff(odb, base_tree, ours_tree)?;
    touched.extend(paths_from_tree_diff(odb, base_tree, theirs_tree)?);
    Ok(touched)
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
    touched: &PathPrefixIndex,
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
    sparse_paths: &PathPrefixIndex,
) -> crate::error::Result<bool> {
    side_tree_has_descendant_at_cached(cache, odb, tree_oid, side, prefix, sparse_paths)
}

#[expect(clippy::too_many_arguments)]
pub(crate) fn compose_merged_tree_oid(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    prefix: &[u8],
    base_oid: &ObjectId,
    ours_oid: &ObjectId,
    theirs_oid: &ObjectId,
    touched: &PathPrefixIndex,
    renamed_away: &HashSet<Vec<u8>>,
    stage0_by_path: &BTreeMap<Vec<u8>, IndexEntry>,
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

        if let Some(entry) = stage0_by_path.get(&fp) {
            out_entries.push(TreeEntry {
                mode: entry.mode,
                name,
                oid: entry.oid,
            });
            continue;
        }

        let mode = o_te.or(t_te).or(b_te).map(|e| e.mode).unwrap_or(MODE_DIR);
        if mode == MODE_DIR {
            let b_sub = b_te.map(|e| e.oid).unwrap_or(*empty_tree);
            let o_sub = o_te.map(|e| e.oid).unwrap_or(*empty_tree);
            let t_sub = t_te.map(|e| e.oid).unwrap_or(*empty_tree);
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
    favor: MergeFavor,
    ws: WhitespaceMergeOptions,
    diff_algorithm: Option<&str>,
    presentation: TreeMergeConflictPresentation<'_>,
) -> crate::error::Result<TreeMergeOutput> {
    use std::collections::HashSet as HSet;

    let odb = repo.odb();
    let ctx = PathMergeContext::new(repo);
    let mut out = crate::index::Index::new();
    let mut conflict_content = BTreeMap::new();
    let mut handled_base: HSet<Vec<u8>> = HSet::new();
    let mut handled_theirs: HSet<Vec<u8>> = HSet::new();

    let mut ours_paths: Vec<Vec<u8>> = touched.iter().cloned().collect();
    for new in ours_new_to_old.keys() {
        ours_paths.push(new.clone());
    }
    ours_paths.sort();
    ours_paths.dedup();

    let theirs_rename_source_by_dest: HashMap<Vec<u8>, Vec<u8>> = theirs_old_to_new
        .iter()
        .map(|(old, new)| (new.clone(), old.clone()))
        .collect();

    // When the other side renamed base paths to `new`, those destinations are also in
    // `touched`. When ours still has the rename source, the source iteration already merges
    // at `new`; skip duplicate passes (issue #1093). Do not skip when ours deleted the source.
    let mut theirs_rename_destinations: HSet<Vec<u8>> = HSet::new();
    for (old, new) in theirs_old_to_new {
        if old != new && touched.contains(old) {
            theirs_rename_destinations.insert(new.clone());
        }
    }

    for op in ours_paths {
        if theirs_rename_destinations.contains(&op) && !ours_new_to_old.contains_key(&op) {
            if let Some(old) = theirs_rename_source_by_dest.get(&op) {
                let ours_still_at_source = ours.contains_key(old)
                    || lookup_index_entry_at_path(cache, odb, &ours_tree, old)
                        .ok()
                        .flatten()
                        .is_some();
                if ours_still_at_source {
                    let ours_present_at_dest = ours.contains_key(&op)
                        || lookup_index_entry_at_path(cache, odb, &ours_tree, &op)
                            .ok()
                            .flatten()
                            .is_some();
                    if !ours_present_at_dest {
                        if theirs.get(&op).is_some()
                            || lookup_index_entry_at_path(cache, odb, &theirs_tree, &op)
                                .ok()
                                .flatten()
                                .is_some()
                        {
                            handled_theirs.insert(op.clone());
                        }
                        continue;
                    }
                }
            }
        }
        let bp = if let Some(old) = ours_new_to_old.get(&op) {
            Some(old.clone())
        } else if base.contains_key(&op) {
            Some(op.clone())
        } else {
            None
        };

        let tp = if let Some(ref bpath) = bp {
            theirs_old_to_new
                .get(bpath)
                .cloned()
                .unwrap_or_else(|| bpath.clone())
        } else {
            op.clone()
        };

        let b = bp.as_ref().and_then(|p| base.get(p).cloned()).or_else(|| {
            bp.as_ref().and_then(|p| {
                lookup_index_entry_at_path(cache, odb, &base_tree, p)
                    .ok()
                    .flatten()
            })
        });
        let o = ours.get(&op).cloned().or_else(|| {
            lookup_index_entry_at_path(cache, odb, &ours_tree, &op)
                .ok()
                .flatten()
        });
        let t = theirs.get(&tp).cloned().or_else(|| {
            lookup_index_entry_at_path(cache, odb, &theirs_tree, &tp)
                .ok()
                .flatten()
        });
        if t.is_some() {
            handled_theirs.insert(tp.clone());
        }
        if let Some(ref p) = bp {
            handled_base.insert(p.clone());
        }

        let out_path = if bp.as_ref().is_some_and(|b| b == &op) && tp != op {
            tp.clone()
        } else {
            op.clone()
        };

        merge_one_path(
            &ctx,
            &mut out,
            &mut conflict_content,
            &out_path,
            b.as_ref(),
            o.as_ref(),
            t.as_ref(),
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    let mut base_paths: Vec<Vec<u8>> = touched.iter().cloned().collect();
    base_paths.sort();
    base_paths.dedup();
    for bp in base_paths {
        if handled_base.contains(&bp) {
            continue;
        }
        let tp = theirs_old_to_new
            .get(&bp)
            .cloned()
            .unwrap_or_else(|| bp.clone());
        let b = base.get(&bp).cloned().or_else(|| {
            lookup_index_entry_at_path(cache, odb, &base_tree, &bp)
                .ok()
                .flatten()
        });
        if b.is_none() {
            continue;
        }
        let t = theirs.get(&tp).cloned().or_else(|| {
            lookup_index_entry_at_path(cache, odb, &theirs_tree, &tp)
                .ok()
                .flatten()
        });
        if t.is_some() {
            handled_theirs.insert(tp.clone());
        }
        merge_one_path(
            &ctx,
            &mut out,
            &mut conflict_content,
            &bp,
            b.as_ref(),
            None,
            t.as_ref(),
            favor,
            ws,
            diff_algorithm,
            presentation,
        )?;
    }

    let mut theirs_paths: Vec<Vec<u8>> = touched.iter().cloned().collect();
    theirs_paths.sort();
    theirs_paths.dedup();
    for tp in theirs_paths {
        if handled_theirs.contains(&tp) {
            continue;
        }
        let t = theirs.get(&tp).cloned().or_else(|| {
            lookup_index_entry_at_path(cache, odb, &theirs_tree, &tp)
                .ok()
                .flatten()
        });
        if t.is_none() {
            continue;
        }
        merge_one_path(
            &ctx,
            &mut out,
            &mut conflict_content,
            &tp,
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
            stage0.insert(entry.path.clone(), entry.clone());
        }
    }
    for entry in output.index.entries() {
        if entry.stage() == 0 {
            continue;
        }
        if output.conflict_content.contains_key(&entry.path) {
            continue;
        }
        if (entry.stage() == 2 || entry.stage() == 3) && !stage0.contains_key(&entry.path) {
            let mut e = entry.clone();
            e.set_stage(0);
            stage0.insert(e.path.clone(), e);
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
