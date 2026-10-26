//! Parsed-tree cache and per-side directory metadata for three-way tree merges.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::merge_trees_walk::PathPrefixIndex;
use crate::objects::{ObjectId, TreeEntry};
use crate::odb::Odb;

const MODE_DIR: u32 = 0o040000;

/// Which root tree a side-specific helper refers to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MergeTreeSide {
    Base,
    Ours,
    Theirs,
}

/// Per-merge cache: parsed tree bodies and directory paths known to contain files.
#[derive(Default)]
pub(crate) struct MergeTreeCache {
    parsed: HashMap<ObjectId, Arc<Vec<TreeEntry>>>,
    dir_with_descendant: [HashSet<Vec<u8>>; 3],
}

impl MergeTreeCache {
    /// Drop parsed trees and path hints so long replay sequences do not retain every step's trees.
    pub(crate) fn clear_retained(&mut self) {
        self.parsed.clear();
        self.dir_with_descendant = [HashSet::new(), HashSet::new(), HashSet::new()];
    }

    pub(crate) fn tree_entries(
        &mut self,
        odb: &Odb,
        oid: &ObjectId,
    ) -> crate::error::Result<Arc<Vec<TreeEntry>>> {
        if let Some(entries) = self.parsed.get(oid) {
            return Ok(Arc::clone(entries));
        }
        // Diff and merge share the ODB's parsed entries; the merge-local map only
        // pins them while walking (the ODB's bounded cache may evict them).
        let entries = odb.cached_tree_entries(oid)?;
        self.parsed.insert(*oid, Arc::clone(&entries));
        Ok(entries)
    }

    pub(crate) fn note_file_path(&mut self, side: MergeTreeSide, path: &[u8]) {
        let idx = side as usize;
        for (i, &b) in path.iter().enumerate() {
            if b == b'/' {
                self.dir_with_descendant[idx].insert(path[..i].to_vec());
            }
        }
    }

    /// True when a noted file lies under `prefix/`. [`Self::note_file_path`] records every
    /// ancestor directory, so any deeper recorded directory implies `prefix` itself is recorded.
    pub(crate) fn dir_has_descendant(&self, side: MergeTreeSide, prefix: &[u8]) -> bool {
        self.dir_with_descendant[side as usize].contains(prefix)
    }
}

pub(crate) fn find_tree_entry<'a>(entries: &'a [TreeEntry], name: &[u8]) -> Option<&'a TreeEntry> {
    entries.iter().find(|e| e.name.as_slice() == name)
}

fn tree_entry_to_index(path: &[u8], te: &TreeEntry) -> crate::index::IndexEntry {
    crate::index::IndexEntry {
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

pub(crate) fn lookup_index_entry_at_path_cached(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    path: &[u8],
) -> crate::error::Result<Option<crate::index::IndexEntry>> {
    if path.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&[u8]> = path
        .split(|&b| b == b'/')
        .filter(|s| !s.is_empty())
        .collect();
    let mut current = *tree_oid;
    for (i, comp) in parts.iter().enumerate() {
        let entries = cache.tree_entries(odb, &current)?;
        let Some(te) = find_tree_entry(&entries, comp) else {
            return Ok(None);
        };
        let is_last = i + 1 == parts.len();
        if is_last {
            return Ok(Some(tree_entry_to_index(path, te)));
        }
        if te.mode != MODE_DIR {
            return Ok(None);
        }
        current = te.oid;
    }
    Ok(None)
}

pub(crate) fn tree_has_entries_with_path_prefix_cached(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    side: MergeTreeSide,
    prefix: &[u8],
) -> crate::error::Result<bool> {
    if cache.dir_has_descendant(side, prefix) {
        return Ok(true);
    }
    // Descend only along `prefix`'s components; `rest` is the part of `prefix` not yet
    // matched. Every tree entry name is compared against `rest` the same way a full-path
    // comparison would, so names that themselves contain `/` are still honoured.
    fn walk(
        cache: &mut MergeTreeCache,
        odb: &Odb,
        tree_oid: &ObjectId,
        rest: &[u8],
    ) -> crate::error::Result<bool> {
        let entries = cache.tree_entries(odb, tree_oid)?;
        let mut descend: Vec<(ObjectId, usize)> = Vec::new();
        for te in entries.iter() {
            let name = te.name.as_slice();
            if name.len() > rest.len() && name.starts_with(rest) && name[rest.len()] == b'/' {
                return Ok(true);
            }
            if te.mode != MODE_DIR {
                continue;
            }
            if name == rest {
                descend.push((te.oid, rest.len()));
            } else if rest.len() > name.len() && rest.starts_with(name) && rest[name.len()] == b'/'
            {
                descend.push((te.oid, name.len() + 1));
            }
        }
        drop(entries);
        for (oid, consumed) in descend {
            if consumed == rest.len() {
                if !cache.tree_entries(odb, &oid)?.is_empty() {
                    return Ok(true);
                }
            } else if walk(cache, odb, &oid, &rest[consumed..])? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    if prefix.is_empty() {
        return Ok(false);
    }
    walk(cache, odb, tree_oid, prefix)
}

pub(crate) fn side_tree_has_descendant_at_cached(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    side: MergeTreeSide,
    prefix: &[u8],
    sparse_paths: &PathPrefixIndex<'_>,
) -> crate::error::Result<bool> {
    if sparse_paths.has_descendant(prefix) {
        return Ok(true);
    }
    if cache.dir_has_descendant(side, prefix) {
        return Ok(true);
    }
    if let Some(entry) = lookup_index_entry_at_path_cached(cache, odb, tree_oid, prefix)? {
        if entry.mode == MODE_DIR {
            let children = cache.tree_entries(odb, &entry.oid)?;
            if !children.is_empty() {
                return Ok(true);
            }
        }
    }
    tree_has_entries_with_path_prefix_cached(cache, odb, tree_oid, side, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::{serialize_tree, ObjectKind};
    use crate::repo::{InitOptions, Repository};

    #[test]
    fn merge_cache_shares_parsed_entries_with_the_odb() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::new()).expect("init");
        let odb = repo.odb();
        let blob = odb.write(ObjectKind::Blob, b"text\n").expect("blob");
        let tree = odb
            .write(
                ObjectKind::Tree,
                &serialize_tree(&[TreeEntry {
                    mode: 0o100644,
                    name: b"file".to_vec(),
                    oid: blob,
                }]),
            )
            .expect("tree");
        let parsed = odb.cached_tree_entries(&tree).expect("parsed");
        let mut cache = MergeTreeCache::default();
        assert!(Arc::ptr_eq(
            &parsed,
            &cache.tree_entries(odb, &tree).expect("merge cache")
        ));
        cache.clear_retained();
        assert!(Arc::ptr_eq(
            &parsed,
            &cache.tree_entries(odb, &tree).expect("reload")
        ));
        assert!(cache.tree_entries(odb, &blob).is_err());
    }
}
