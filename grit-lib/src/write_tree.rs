//! Build tree objects from index entries (`git write-tree` core logic).

use std::collections::BTreeMap;

use crate::error::Result;
use crate::index::{
    CacheTreeNode, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK,
    MODE_TREE,
};
use crate::objects::{parse_tree, serialize_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry};
use crate::odb::Odb;

fn ensure_empty_blob_for_intent_to_add(odb: &Odb, index: &Index) -> Result<()> {
    if index
        .entries
        .iter()
        .any(|e| e.stage() == 0 && e.intent_to_add())
    {
        let _ = odb.write(ObjectKind::Blob, b"")?;
    }
    Ok(())
}

/// Build and write tree object(s) from index entries and return the tree OID.
///
/// The `prefix` argument optionally limits the write to a subtree path.
/// Like [`write_tree_from_index`], but only index entries whose path is listed in `paths`
/// (repository-relative, as stored in the index) are included in the tree.
pub fn write_tree_from_index_subset(
    odb: &Odb,
    index: &Index,
    paths: &std::collections::HashSet<Vec<u8>>,
) -> Result<ObjectId> {
    ensure_empty_blob_for_intent_to_add(odb, index)?;

    let mut entries: Vec<&IndexEntry> = index
        .entries
        .iter()
        .filter(|entry| {
            entry.stage() == 0
                && !entry.intent_to_add()
                && entry.mode != MODE_TREE
                && paths.contains(&entry.path)
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
    build_tree(odb, &entries, b"")
}

/// Build and write tree object(s) from index entries and return the tree OID.
pub fn write_tree_from_index(odb: &Odb, index: &Index, prefix: &str) -> Result<ObjectId> {
    ensure_empty_blob_for_intent_to_add(odb, index)?;

    let prefix_bytes = prefix.as_bytes();
    let mut entries: Vec<&IndexEntry> = index
        .entries
        .iter()
        .filter(|entry| {
            entry.stage() == 0
                && !entry.intent_to_add()
                && entry.mode != MODE_TREE
                && entry.path.starts_with(prefix_bytes)
        })
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
    if prefix_bytes.is_empty() {
        if let Some(mut cache_root) = index.cache_tree.clone() {
            update_cache_tree_node(odb, &mut cache_root, b"", &entries)?;
            if let Some(oid) = cache_root.oid {
                return Ok(oid);
            }
        }
    }
    build_tree(odb, &entries, prefix_bytes)
}

/// Build a valid cache-tree extension from the index and write any missing tree objects.
///
/// # Errors
///
/// Returns an error if tree object creation fails.
pub fn build_cache_tree_from_index(odb: &Odb, index: &Index) -> Result<CacheTreeNode> {
    ensure_empty_blob_for_intent_to_add(odb, index)?;
    let mut entries: Vec<&IndexEntry> = index
        .entries
        .iter()
        .filter(|entry| entry.stage() == 0 && !entry.intent_to_add() && entry.mode != MODE_TREE)
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path).then_with(|| a.stage().cmp(&b.stage())));
    let mut root = CacheTreeNode {
        name: Vec::new(),
        entry_count: -1,
        oid: None,
        children: Vec::new(),
    };
    update_cache_tree_node(odb, &mut root, b"", &entries)?;
    Ok(root)
}

/// Build a cache-tree directly from a **tree object**, preserving Git's raw `entry_count`
/// semantics. Unlike [`build_cache_tree_from_index`], the per-node `entry_count` is the recursive
/// number of non-tree entries *as recorded in the tree* — duplicate path entries are counted
/// separately, exactly as upstream Git's cache-tree does after reading such a tree into the index.
///
/// This is the cache-tree real Git attaches after `read-tree`/`reset`/`checkout` populate the index
/// from a tree. For a tree with duplicate entries (`t4058-diff-duplicates`) the resulting
/// `entry_count` exceeds the number of (deduplicated) index entries, so [`verify_cache_tree`] later
/// reports "corrupted cache-tree has entries not present in index".
pub fn build_cache_tree_from_tree(odb: &Odb, tree_oid: &ObjectId) -> Result<CacheTreeNode> {
    build_cache_tree_from_tree_named(odb, tree_oid, Vec::new())
}

fn build_cache_tree_from_tree_named(
    odb: &Odb,
    tree_oid: &ObjectId,
    name: Vec<u8>,
) -> Result<CacheTreeNode> {
    let obj = odb.read(tree_oid)?;
    let entries = parse_tree(&obj.data)?;

    let mut entry_count: i32 = 0;
    let mut children: Vec<CacheTreeNode> = Vec::new();
    for te in entries {
        if te.mode == MODE_TREE {
            let child = build_cache_tree_from_tree_named(odb, &te.oid, te.name.clone())?;
            entry_count = entry_count.saturating_add(child.entry_count.max(0));
            children.push(child);
        } else {
            entry_count = entry_count.saturating_add(1);
        }
    }
    // Cache-tree children are stored sorted by name (plain byte order, no tree/dir suffix rule).
    children.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(CacheTreeNode::valid(name, entry_count, *tree_oid, children))
}

/// Walk a cache-tree against `index`, mirroring Git's `verify_one` / `cache_tree_verify`.
///
/// Returns an error describing the first inconsistency. The only condition exercised by
/// `t4058-diff-duplicates` is a node whose `entry_count` runs past the end of the index
/// (`entry_count + pos > cache_nr`), which yields the exact upstream message
/// "corrupted cache-tree has entries not present in index".
///
/// This is gated by callers behind `GIT_TEST_CHECK_CACHE_TREE`, matching upstream's
/// `write_locked_index`.
pub fn verify_cache_tree(index: &Index) -> Result<()> {
    let Some(root) = index.cache_tree.as_ref() else {
        return Ok(());
    };
    // Stage-0, non-tree entries in canonical (path) order — the layout the cache-tree indexes.
    let mut cache: Vec<&IndexEntry> = index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && e.mode != MODE_TREE)
        .collect();
    cache.sort_by(|a, b| a.path.cmp(&b.path));
    verify_cache_tree_one(root, &cache, &mut Vec::new())
}

fn verify_cache_tree_one(
    node: &CacheTreeNode,
    cache: &[&IndexEntry],
    path: &mut Vec<u8>,
) -> Result<()> {
    let len = path.len();
    for child in &node.children {
        path.extend_from_slice(&child.name);
        path.push(b'/');
        verify_cache_tree_one(child, cache, path)?;
        path.truncate(len);
    }

    if node.entry_count < 0 {
        return Ok(());
    }

    // Position of the first cache entry whose path is at/under this node's directory prefix.
    let pos = match cache.binary_search_by(|e| e.path.as_slice().cmp(path.as_slice())) {
        Ok(p) => p,
        Err(p) => p,
    };

    if (node.entry_count as usize).saturating_add(pos) > cache.len() {
        return Err(crate::error::Error::CacheTreeCorrupt);
    }
    Ok(())
}

fn build_tree(odb: &Odb, entries: &[&IndexEntry], dir_prefix: &[u8]) -> Result<ObjectId> {
    let mut children: BTreeMap<Vec<u8>, ChildKind> = BTreeMap::new();

    for entry in entries {
        let path = &entry.path;
        let rel = if dir_prefix.is_empty() {
            path.as_slice()
        } else {
            path.strip_prefix(dir_prefix)
                .and_then(|suffix| suffix.strip_prefix(b"/"))
                .unwrap_or(path.as_slice())
        };

        if let Some(slash_pos) = rel.iter().position(|&byte| byte == b'/') {
            let child_name = rel[..slash_pos].to_vec();
            let sub_prefix = if dir_prefix.is_empty() {
                child_name.clone()
            } else {
                let mut sub_prefix = dir_prefix.to_vec();
                sub_prefix.push(b'/');
                sub_prefix.extend_from_slice(&child_name);
                sub_prefix
            };
            children
                .entry(child_name)
                .or_insert_with(|| ChildKind::Tree(sub_prefix, Vec::new()))
                .push_entry(entry);
        } else {
            children
                .entry(rel.to_vec())
                .or_insert_with(|| ChildKind::Blob {
                    mode: canonicalize_blob_mode(entry.mode),
                    oid: entry.oid,
                });
        }
    }

    let mut tree_entries = Vec::with_capacity(children.len());
    for (name, child) in children {
        match child {
            ChildKind::Blob { mode, oid } => tree_entries.push(TreeEntry { mode, name, oid }),
            ChildKind::Tree(sub_prefix, sub_entries) => {
                let sub_oid = build_tree(odb, &sub_entries, &sub_prefix)?;
                tree_entries.push(TreeEntry {
                    mode: MODE_TREE,
                    name,
                    oid: sub_oid,
                });
            }
        }
    }

    tree_entries.sort_by(|a, b| {
        let a_tree = a.mode == MODE_TREE;
        let b_tree = b.mode == MODE_TREE;
        tree_entry_cmp(&a.name, a_tree, &b.name, b_tree)
    });

    let data = serialize_tree(&tree_entries);
    ensure_tree_entries_exist(odb, &tree_entries)?;
    write_tree_object(odb, &data)
}

fn cache_tree_subtree_mut<'a>(node: &'a mut CacheTreeNode, name: &[u8]) -> &'a mut CacheTreeNode {
    match node
        .children
        .binary_search_by(|c| c.name.as_slice().cmp(name))
    {
        Ok(pos) => &mut node.children[pos],
        Err(insert_at) => {
            node.children.insert(
                insert_at,
                CacheTreeNode {
                    name: name.to_vec(),
                    entry_count: -1,
                    oid: None,
                    children: Vec::new(),
                },
            );
            &mut node.children[insert_at]
        }
    }
}

/// Update `node` to reflect `entries` under `dir_prefix`, reusing valid subtrees when possible
/// (Git `cache_tree_update` / `update_one`).
fn update_cache_tree_node(
    odb: &Odb,
    node: &mut CacheTreeNode,
    dir_prefix: &[u8],
    entries: &[&IndexEntry],
) -> Result<()> {
    if node.is_valid() {
        if let Some(oid) = node.oid {
            if odb.exists(&oid) {
                return Ok(());
            }
        }
    }

    let mut children: BTreeMap<Vec<u8>, ChildKind> = BTreeMap::new();

    for entry in entries {
        let path = &entry.path;
        let rel = if dir_prefix.is_empty() {
            path.as_slice()
        } else {
            path.strip_prefix(dir_prefix)
                .and_then(|suffix| suffix.strip_prefix(b"/"))
                .unwrap_or(path.as_slice())
        };

        if let Some(slash_pos) = rel.iter().position(|&byte| byte == b'/') {
            let child_name = rel[..slash_pos].to_vec();
            let sub_prefix = if dir_prefix.is_empty() {
                child_name.clone()
            } else {
                let mut sub_prefix = dir_prefix.to_vec();
                sub_prefix.push(b'/');
                sub_prefix.extend_from_slice(&child_name);
                sub_prefix
            };
            children
                .entry(child_name)
                .or_insert_with(|| ChildKind::Tree(sub_prefix, Vec::new()))
                .push_entry(entry);
        } else {
            children
                .entry(rel.to_vec())
                .or_insert_with(|| ChildKind::Blob {
                    mode: canonicalize_blob_mode(entry.mode),
                    oid: entry.oid,
                });
        }
    }

    let mut used_child_names: Vec<Vec<u8>> = Vec::new();
    let mut tree_entries = Vec::with_capacity(children.len());
    let mut cache_children = Vec::new();

    for (child_name, child) in children {
        used_child_names.push(child_name.clone());
        match child {
            ChildKind::Blob { mode, oid } => tree_entries.push(TreeEntry {
                mode,
                name: child_name,
                oid,
            }),
            ChildKind::Tree(sub_prefix, sub_entries) => {
                let child_node = cache_tree_subtree_mut(node, &child_name);
                update_cache_tree_node(odb, child_node, &sub_prefix, &sub_entries)?;
                let oid = child_node.oid.ok_or_else(|| {
                    crate::error::Error::IndexError("cache-tree child missing oid".to_owned())
                })?;
                tree_entries.push(TreeEntry {
                    mode: MODE_TREE,
                    name: child_name.clone(),
                    oid,
                });
                cache_children.push(child_node.clone());
            }
        }
    }

    node.children
        .retain(|c| used_child_names.iter().any(|n| n == &c.name));

    tree_entries.sort_by(|a, b| {
        let a_tree = a.mode == MODE_TREE;
        let b_tree = b.mode == MODE_TREE;
        tree_entry_cmp(&a.name, a_tree, &b.name, b_tree)
    });
    cache_children.sort_by(|a, b| a.name.cmp(&b.name));

    let data = serialize_tree(&tree_entries);
    ensure_tree_entries_exist(odb, &tree_entries)?;
    let oid = write_tree_object(odb, &data)?;
    node.entry_count = i32::try_from(entries.len()).unwrap_or(i32::MAX);
    node.oid = Some(oid);
    node.children = cache_children;
    Ok(())
}

/// Build a tree for a **partial** commit: paths listed in `paths_from_index` (repository-relative,
/// UTF-8 path bytes) are taken from `index`; every other path is copied from `base_tree_oid`
/// (typically `HEAD^{tree}`).
///
/// This matches Git's behaviour when committing with pathspecs while the index contains additional
/// staged paths: the commit tree merges `HEAD` with only the pathspec-selected index updates.
pub fn write_tree_partial_from_index(
    odb: &Odb,
    index: &Index,
    base_tree_oid: &ObjectId,
    paths_from_index: &std::collections::HashSet<Vec<u8>>,
) -> Result<ObjectId> {
    let _ = odb.write(ObjectKind::Blob, b"");

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

    fn subtree_affected(paths_from_index: &std::collections::HashSet<Vec<u8>>, dir: &[u8]) -> bool {
        paths_from_index
            .iter()
            .any(|p| p == dir || (p.starts_with(dir) && p.get(dir.len()) == Some(&b'/')))
    }

    fn index_has_entry_under(index: &Index, dir: &[u8]) -> bool {
        index.entries.iter().any(|entry| {
            entry.stage() == 0
                && !entry.intent_to_add()
                && entry.mode != MODE_TREE
                && entry.path.starts_with(dir)
                && entry.path.get(dir.len()) == Some(&b'/')
        })
    }

    fn merge_level(
        odb: &Odb,
        index: &Index,
        base_tree_oid: &ObjectId,
        prefix: &[u8],
        paths_from_index: &std::collections::HashSet<Vec<u8>>,
    ) -> Result<ObjectId> {
        let base_obj = odb.read(base_tree_oid)?;
        let base_entries = parse_tree(&base_obj.data)?;

        let mut by_name: BTreeMap<Vec<u8>, TreeEntry> = BTreeMap::new();
        for te in base_entries {
            let fp = full_path(prefix, &te.name);
            if !subtree_affected(paths_from_index, &fp) {
                by_name.insert(te.name.clone(), te);
            } else if te.mode == MODE_TREE {
                if paths_from_index.contains(&fp) && !index_has_entry_under(index, &fp) {
                    continue;
                }
                let sub_oid = merge_level(odb, index, &te.oid, &fp, paths_from_index)?;
                by_name.insert(
                    te.name.clone(),
                    TreeEntry {
                        mode: MODE_TREE,
                        name: te.name,
                        oid: sub_oid,
                    },
                );
            } else if paths_from_index.contains(&fp) {
                if let Some(ie) = index.entries.iter().find(|e| {
                    e.stage() == 0 && !e.intent_to_add() && e.mode != MODE_TREE && e.path == fp
                }) {
                    by_name.insert(
                        te.name.clone(),
                        TreeEntry {
                            mode: canonicalize_blob_mode(ie.mode),
                            name: te.name,
                            oid: ie.oid,
                        },
                    );
                }
                // No index entry: path was removed — omit from the merged tree.
            } else {
                by_name.insert(te.name.clone(), te);
            }
        }

        for ie in &index.entries {
            if ie.stage() != 0 || ie.intent_to_add() || ie.mode == MODE_TREE {
                continue;
            }
            if !paths_from_index.contains(&ie.path) {
                continue;
            }
            let rel = if prefix.is_empty() {
                ie.path.as_slice()
            } else if ie.path.starts_with(prefix) && ie.path.get(prefix.len()) == Some(&b'/') {
                &ie.path[prefix.len() + 1..]
            } else {
                continue;
            };
            if rel.is_empty() {
                continue;
            }
            if let Some(slash) = rel.iter().position(|&b| b == b'/') {
                let dir_name = rel[..slash].to_vec();
                if by_name.contains_key(&dir_name) {
                    continue;
                }
                let sub_prefix = full_path(prefix, &dir_name);
                let sub_oid =
                    write_tree_from_index(odb, index, &String::from_utf8_lossy(&sub_prefix))?;
                by_name.insert(
                    dir_name.clone(),
                    TreeEntry {
                        mode: MODE_TREE,
                        name: dir_name,
                        oid: sub_oid,
                    },
                );
            } else {
                let name = rel.to_vec();
                if !by_name.contains_key(&name) {
                    by_name.insert(
                        name.clone(),
                        TreeEntry {
                            mode: canonicalize_blob_mode(ie.mode),
                            name,
                            oid: ie.oid,
                        },
                    );
                }
            }
        }

        let mut out: Vec<TreeEntry> = by_name.into_values().collect();
        out.sort_by(|a, b| {
            let a_tree = a.mode == MODE_TREE;
            let b_tree = b.mode == MODE_TREE;
            tree_entry_cmp(&a.name, a_tree, &b.name, b_tree)
        });
        let data = serialize_tree(&out);
        ensure_tree_entries_exist(odb, &out)?;
        write_tree_object(odb, &data)
    }

    merge_level(odb, index, base_tree_oid, b"", paths_from_index)
}

/// Store a reconstructed tree through [`Odb::write`] so duplicate tree OIDs are freshened.
///
/// Valid cache-tree subtrees skip reconstruction entirely; this path runs only for trees
/// rebuilt from index entries.
fn write_tree_object(odb: &Odb, data: &[u8]) -> Result<ObjectId> {
    odb.write(ObjectKind::Tree, data)
}

/// Verify tree child OIDs are reachable in the ODB (Git cache-tree `odb_has_object`).
///
/// Gitlink entries reference submodule commits and are not checked against the parent ODB.
fn ensure_tree_entries_exist(odb: &Odb, tree_entries: &[TreeEntry]) -> Result<()> {
    for entry in tree_entries {
        if entry.mode == MODE_GITLINK {
            continue;
        }
        if !odb.exists(&entry.oid) {
            return Err(crate::error::Error::ObjectNotFound(entry.oid.to_hex()));
        }
    }
    Ok(())
}

fn canonicalize_blob_mode(mode: u32) -> u32 {
    match mode & 0o170000 {
        0o120000 => MODE_SYMLINK,
        0o160000 => MODE_GITLINK,
        0o100000 => {
            if mode & 0o111 != 0 {
                MODE_EXECUTABLE
            } else {
                MODE_REGULAR
            }
        }
        _ => MODE_REGULAR,
    }
}

enum ChildKind<'a> {
    Blob { mode: u32, oid: ObjectId },
    Tree(Vec<u8>, Vec<&'a IndexEntry>),
}

impl<'a> ChildKind<'a> {
    fn push_entry(&mut self, entry: &'a IndexEntry) {
        if let Self::Tree(_, entries) = self {
            entries.push(entry);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::expect_used, clippy::unwrap_used)]

    use super::*;
    use crate::index::{IndexEntry, MODE_EXECUTABLE, MODE_REGULAR, MODE_SYMLINK, MODE_TREE};
    use crate::objects::parse_tree;
    use tempfile::TempDir;

    fn entry(path: &str, mode: u32, oid: ObjectId) -> IndexEntry {
        IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode,
            uid: 0,
            gid: 0,
            size: 0,
            oid,
            flags: path.len().min(0xFFF) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        }
    }

    #[test]
    fn writes_sorted_tree_with_canonical_modes() {
        let temp_dir = TempDir::new().unwrap();
        let odb = Odb::new(temp_dir.path());

        let oid_a = odb.write(ObjectKind::Blob, b"a").unwrap();
        let oid_exec = odb.write(ObjectKind::Blob, b"exec").unwrap();
        let oid_link = odb.write(ObjectKind::Blob, b"target").unwrap();

        let mut index = Index::new();
        index.add_or_replace(entry("bin/run.sh", 0o100777, oid_exec));
        index.add_or_replace(entry("link", 0o120777, oid_link));
        index.add_or_replace(entry("a.txt", 0o100664, oid_a));

        let root_oid = write_tree_from_index(&odb, &index, "").unwrap();
        let root_tree_obj = odb.read(&root_oid).unwrap();
        let root_entries = parse_tree(&root_tree_obj.data).unwrap();

        assert_eq!(root_entries.len(), 3);
        assert_eq!(root_entries[0].name, b"a.txt");
        assert_eq!(root_entries[0].mode, MODE_REGULAR);
        assert_eq!(root_entries[1].name, b"bin");
        assert_eq!(root_entries[1].mode, MODE_TREE);
        assert_eq!(root_entries[2].name, b"link");
        assert_eq!(root_entries[2].mode, MODE_SYMLINK);

        let bin_tree_obj = odb.read(&root_entries[1].oid).unwrap();
        let bin_entries = parse_tree(&bin_tree_obj.data).unwrap();
        assert_eq!(bin_entries.len(), 1);
        assert_eq!(bin_entries[0].name, b"run.sh");
        assert_eq!(bin_entries[0].mode, MODE_EXECUTABLE);
    }

    #[test]
    fn write_tree_does_not_touch_entry_blobs() {
        use filetime::FileTime;
        use std::fs;
        use std::thread;
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let odb = Odb::new(temp_dir.path());

        let oid_a = odb.write(ObjectKind::Blob, b"a").unwrap();
        let oid_b = odb.write(ObjectKind::Blob, b"b").unwrap();

        let path_a = odb.object_path(&oid_a);
        let path_b = odb.object_path(&oid_b);
        thread::sleep(Duration::from_millis(50));
        let stamp_a = FileTime::from_last_modification_time(&fs::metadata(&path_a).unwrap());
        let stamp_b = FileTime::from_last_modification_time(&fs::metadata(&path_b).unwrap());
        filetime::set_file_mtime(&path_a, stamp_a).unwrap();
        filetime::set_file_mtime(&path_b, stamp_b).unwrap();

        let mut index = Index::new();
        index.add_or_replace(entry("a.txt", 0o100644, oid_a));
        index.add_or_replace(entry("b.txt", 0o100644, oid_b));

        write_tree_from_index(&odb, &index, "").unwrap();

        let after_a = FileTime::from_last_modification_time(&fs::metadata(&path_a).unwrap());
        let after_b = FileTime::from_last_modification_time(&fs::metadata(&path_b).unwrap());
        assert_eq!(after_a, stamp_a, "write-tree must not freshen index blob a");
        assert_eq!(after_b, stamp_b, "write-tree must not freshen index blob b");
    }

    #[test]
    fn write_tree_freshens_reconstructed_existing_tree() {
        use filetime::FileTime;
        use std::fs;
        use std::thread;
        use std::time::Duration;

        let temp_dir = TempDir::new().unwrap();
        let odb = Odb::new(temp_dir.path());

        let oid_a = odb.write(ObjectKind::Blob, b"a").unwrap();
        let oid_b = odb.write(ObjectKind::Blob, b"b").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("a.txt", 0o100644, oid_a));
        index.add_or_replace(entry("b.txt", 0o100644, oid_b));

        let root_oid = write_tree_from_index(&odb, &index, "").unwrap();
        let tree_path = odb.object_path(&root_oid);

        let path_a = odb.object_path(&oid_a);
        let path_b = odb.object_path(&oid_b);
        thread::sleep(Duration::from_millis(50));
        let stamp_a = FileTime::from_last_modification_time(&fs::metadata(&path_a).unwrap());
        let stamp_b = FileTime::from_last_modification_time(&fs::metadata(&path_b).unwrap());
        filetime::set_file_mtime(&path_a, stamp_a).unwrap();
        filetime::set_file_mtime(&path_b, stamp_b).unwrap();

        let two_days_ago = FileTime::from_unix_time(
            filetime::FileTime::now().unix_seconds() - time::Duration::days(2).whole_seconds(),
            0,
        );
        filetime::set_file_mtime(&tree_path, two_days_ago).unwrap();
        let aged = FileTime::from_last_modification_time(&fs::metadata(&tree_path).unwrap());

        write_tree_from_index(&odb, &index, "").unwrap();

        let after_tree = FileTime::from_last_modification_time(&fs::metadata(&tree_path).unwrap());
        assert!(
            after_tree > aged,
            "reconstructed tree must be freshened (t6501 same-tree)"
        );
        let after_a = FileTime::from_last_modification_time(&fs::metadata(&path_a).unwrap());
        let after_b = FileTime::from_last_modification_time(&fs::metadata(&path_b).unwrap());
        assert_eq!(after_a, stamp_a);
        assert_eq!(after_b, stamp_b);
    }
}
