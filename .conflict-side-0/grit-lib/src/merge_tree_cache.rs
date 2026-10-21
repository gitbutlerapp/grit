//! Parsed-tree cache and per-side directory metadata for three-way tree merges.

use std::collections::{HashMap, HashSet};

use crate::objects::{parse_tree, ObjectId, ObjectKind, TreeEntry};
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
    parsed: HashMap<ObjectId, Vec<TreeEntry>>,
    dir_with_descendant: [HashSet<Vec<u8>>; 3],
}

impl MergeTreeCache {
    pub(crate) fn tree_entries(
        &mut self,
        odb: &Odb,
        oid: &ObjectId,
    ) -> crate::error::Result<&[TreeEntry]> {
        if !self.parsed.contains_key(oid) {
            let obj = odb.read(oid)?;
            let entries = if obj.kind == ObjectKind::Tree {
                parse_tree(&obj.data)?
            } else {
                Vec::new()
            };
            self.parsed.insert(*oid, entries);
        }
        Ok(self.parsed.get(oid).map_or(&[][..], |v| v.as_slice()))
    }

    pub(crate) fn note_file_path(&mut self, side: MergeTreeSide, path: &[u8]) {
        let idx = side as usize;
        for (i, &b) in path.iter().enumerate() {
            if b == b'/' {
                self.dir_with_descendant[idx].insert(path[..i].to_vec());
            }
        }
    }

    pub(crate) fn dir_has_descendant(&self, side: MergeTreeSide, prefix: &[u8]) -> bool {
        let set = &self.dir_with_descendant[side as usize];
        if set.contains(prefix) {
            return true;
        }
        let need = prefix.len().saturating_add(1);
        set.iter().any(|p| {
            p.len() >= need
                && p.starts_with(prefix)
                && p.as_slice().get(prefix.len()) == Some(&b'/')
        })
    }
}

pub(crate) fn find_tree_entry<'a>(entries: &'a [TreeEntry], name: &[u8]) -> Option<&'a TreeEntry> {
    entries.iter().find(|e| e.name.as_slice() == name)
}

fn full_path(prefix: &[u8], name: &[u8]) -> Vec<u8> {
    if prefix.is_empty() {
        name.to_vec()
    } else {
        let mut p = prefix.to_vec();
        p.push(b'/');
        p.extend_from_slice(name);
        p
    }
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
        let Some(te) = find_tree_entry(entries, comp) else {
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
    fn walk(
        cache: &mut MergeTreeCache,
        odb: &Odb,
        tree_oid: &ObjectId,
        current: &[u8],
        prefix: &[u8],
    ) -> crate::error::Result<bool> {
        let entries: Vec<TreeEntry> = cache.tree_entries(odb, tree_oid)?.to_vec();
        for te in entries {
            let fp = full_path(current, &te.name);
            if fp.len() > prefix.len()
                && fp.starts_with(prefix)
                && fp.as_slice().get(prefix.len()) == Some(&b'/')
            {
                return Ok(true);
            }
            if te.mode == MODE_DIR
                && (fp.starts_with(prefix) || prefix.starts_with(&fp))
                && walk(cache, odb, &te.oid, &fp, prefix)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    walk(cache, odb, tree_oid, b"", prefix)
}

pub(crate) fn side_tree_has_descendant_at_cached(
    cache: &mut MergeTreeCache,
    odb: &Odb,
    tree_oid: &ObjectId,
    side: MergeTreeSide,
    prefix: &[u8],
    sparse_paths: &HashSet<Vec<u8>>,
) -> crate::error::Result<bool> {
    let need = prefix.len().saturating_add(1);
    if sparse_paths
        .iter()
        .any(|p| p.len() >= need && p.starts_with(prefix) && p[prefix.len()] == b'/')
    {
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
