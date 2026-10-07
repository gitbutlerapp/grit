//! Build tree objects from index entries (`git write-tree` core logic).

use std::collections::BTreeMap;

use crate::error::{Error, Result};
use crate::index::{
    CacheTreeNode, Index, IndexEntry, MODE_EXECUTABLE, MODE_GITLINK, MODE_REGULAR, MODE_SYMLINK,
    MODE_TREE,
};
use crate::objects::{parse_tree, serialize_tree, tree_entry_cmp, ObjectId, ObjectKind, TreeEntry};
use crate::odb::{Odb, WriteOptions};

/// How [`cache_tree_update`] persists rebuilt tree objects.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum WriteTreePersistence {
    /// Write missing tree objects to the object store.
    #[default]
    Write,
    /// Compute tree OIDs without writing (dry run).
    DryRun,
    /// Reuse an existing OID when serialized tree bytes already exist.
    Repair,
}

/// Options for [`cache_tree_update`] and [`write_tree_update_index`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WriteTreeFlags {
    /// Allow missing blob OIDs when building trees.
    pub missing_ok: bool,
    /// Discard the index cache-tree and rebuild from scratch.
    pub ignore_cache_tree: bool,
    /// How tree objects are stored or hashed.
    pub persistence: WriteTreePersistence,
    /// Skip mtime freshen on existing objects when writing.
    pub silent: bool,
}

impl WriteTreeFlags {
    /// Silent refresh that reuses existing tree OIDs when bytes match.
    #[must_use]
    pub fn silent_repair() -> Self {
        Self {
            silent: true,
            persistence: WriteTreePersistence::Repair,
            missing_ok: false,
            ignore_cache_tree: false,
        }
    }

    /// Like [`Self::default`], but tree writes use [`WriteOptions::silent`] so existing
    /// objects are not freshened (used when refreshing the index cache-tree after commit).
    #[must_use]
    pub fn silent() -> Self {
        Self {
            silent: true,
            ..Self::default()
        }
    }
}

/// Returns whether `left` and `right` have the same stage-0 rows that feed cache-tree / write-tree.
///
/// Used after commit when the on-disk index may have been mutated by hooks while the in-memory
/// index (and its cache-tree) reflects the tree that was actually committed.
#[must_use]
pub fn index_cache_tree_inputs_match(left: &Index, right: &Index) -> bool {
    let rows_left = cache_tree_index_rows(left);
    let rows_right = cache_tree_index_rows(right);
    if rows_left.len() != rows_right.len() {
        return false;
    }
    rows_left.iter().zip(rows_right.iter()).all(|(a, b)| {
        a.path == b.path
            && a.mode == b.mode
            && a.oid == b.oid
            && a.intent_to_add() == b.intent_to_add()
            && a.is_sparse_directory_placeholder() == b.is_sparse_directory_placeholder()
    })
}

/// Returns whether every node in `node` is valid and its tree object exists in `odb`.
#[must_use]
pub fn cache_tree_fully_valid(odb: &Odb, node: Option<&CacheTreeNode>) -> bool {
    let Some(it) = node else {
        return false;
    };
    if it.entry_count < 0 {
        return false;
    }
    let Some(oid) = it.oid else {
        return false;
    };
    if !odb.exists(&oid) {
        return false;
    }
    it.children
        .iter()
        .all(|child| cache_tree_fully_valid(odb, Some(child)))
}

/// Update the index cache-tree extension and write any missing tree objects.
///
/// On success the index's [`Index::cache_tree`] reflects the current stage-0 entries.
///
/// # Errors
///
/// Returns an error when the index has unmerged entries, path/file conflicts, or missing objects.
pub fn cache_tree_update(odb: &Odb, index: &mut Index, flags: WriteTreeFlags) -> Result<()> {
    verify_index_for_cache_tree(index, flags.silent)?;

    if flags.ignore_cache_tree {
        index.clear_cache_tree();
    }

    let mut root = index
        .cache_tree
        .take()
        .unwrap_or_else(empty_cache_tree_root);

    let rows: Vec<&IndexEntry> = cache_tree_index_rows(index);
    let mut skipped = 0i32;
    let consumed = rebuild_directory_node(odb, &mut root, &rows, b"", 0, &mut skipped, flags)?;
    if consumed < 0 {
        return Err(Error::IndexError("cache-tree update failed".to_owned()));
    }
    index.cache_tree_root = root.oid.filter(|_| root.entry_count >= 0);
    index.cache_tree = Some(root);
    Ok(())
}

/// Write the index as a tree, updating the cache-tree in place when needed.
///
/// When the existing cache-tree is fully valid and `prefix` is empty, this returns the cached
/// root OID without rewriting tree objects.
pub fn write_tree_update_index(
    odb: &Odb,
    index: &mut Index,
    prefix: &str,
    flags: WriteTreeFlags,
) -> Result<ObjectId> {
    ensure_empty_blob_for_intent_to_add(odb, index)?;

    let was_valid =
        !flags.ignore_cache_tree && cache_tree_fully_valid(odb, index.cache_tree.as_ref());

    if !was_valid {
        cache_tree_update(odb, index, flags)?;
    }

    if prefix.is_empty() {
        let root = index.cache_tree.as_ref().ok_or_else(|| {
            Error::IndexError("write-tree: missing cache-tree after update".to_owned())
        })?;
        let oid = root.oid.ok_or_else(|| {
            Error::IndexError("write-tree: cache-tree root has no oid".to_owned())
        })?;
        return Ok(oid);
    }

    let subtree = find_cache_tree_subtree(index.cache_tree.as_ref(), prefix.as_bytes())
        .ok_or_else(|| Error::IndexError(format!("write-tree: invalid prefix '{prefix}'")))?;
    let oid = subtree
        .oid
        .ok_or_else(|| Error::IndexError(format!("write-tree: invalid prefix '{prefix}'")))?;
    Ok(oid)
}

fn empty_cache_tree_root() -> CacheTreeNode {
    CacheTreeNode {
        name: Vec::new(),
        entry_count: -1,
        oid: None,
        children: Vec::new(),
    }
}

/// Stage-0 index rows that participate in cache-tree / write-tree (includes sparse-directory placeholders).
fn cache_tree_index_rows(index: &Index) -> Vec<&IndexEntry> {
    index
        .entries
        .iter()
        .filter(|e| e.stage() == 0 && (e.mode != MODE_TREE || e.is_sparse_directory_placeholder()))
        .collect()
}

fn verify_index_for_cache_tree(index: &Index, _silent: bool) -> Result<()> {
    if index.entries.iter().any(|e| e.stage() != 0) {
        return Err(Error::IndexUnmerged);
    }
    let rows = cache_tree_index_rows(index);
    for pair in rows.windows(2) {
        let shorter = &pair[0].path;
        let longer = &pair[1].path;
        if longer.len() > shorter.len()
            && longer.get(shorter.len()) == Some(&b'/')
            && longer.starts_with(shorter.as_slice())
        {
            return Err(Error::IndexPathPrefixConflict {
                directory: path_buf_from_bytes(shorter),
                file: path_buf_from_bytes(longer),
            });
        }
    }
    Ok(())
}

fn path_buf_from_bytes(bytes: &[u8]) -> std::path::PathBuf {
    std::path::PathBuf::from(String::from_utf8_lossy(bytes).into_owned())
}

fn find_cache_tree_subtree<'a>(
    root: Option<&'a CacheTreeNode>,
    prefix: &[u8],
) -> Option<&'a CacheTreeNode> {
    let mut it = root?;
    let mut path = prefix;
    while !path.is_empty() {
        if path[0] == b'/' {
            path = &path[1..];
            continue;
        }
        let slash = path.iter().position(|&b| b == b'/').unwrap_or(path.len());
        let component = &path[..slash];
        path = &path[slash..];
        it = it
            .children
            .iter()
            .find(|c| c.name.as_slice() == component)?;
    }
    Some(it)
}

fn find_or_create_child<'a>(
    node: &'a mut CacheTreeNode,
    name: &[u8],
    create: bool,
) -> Option<&'a mut CacheTreeNode> {
    if let Some(pos) = node.children.iter().position(|c| c.name.as_slice() == name) {
        return Some(&mut node.children[pos]);
    }
    if !create {
        return None;
    }
    node.children.push(CacheTreeNode {
        name: name.to_vec(),
        entry_count: -1,
        oid: None,
        children: Vec::new(),
    });
    node.children.last_mut()
}

/// Rebuild one cache-tree node and the index rows it covers.
fn rebuild_directory_node(
    odb: &Odb,
    node: &mut CacheTreeNode,
    rows: &[&IndexEntry],
    anchor: &[u8],
    prefix_len: usize,
    extra_skip: &mut i32,
    flags: WriteTreeFlags,
) -> Result<i32> {
    use std::collections::HashMap;

    *extra_skip = 0;

    if let Some(head) = rows.first() {
        if head.is_sparse_directory_placeholder()
            && head.path.len() == prefix_len
            && entry_matches_prefix(&head.path, anchor, prefix_len)
        {
            node.entry_count = 1;
            node.oid = Some(head.oid);
            return Ok(1);
        }
    }

    if cached_node_still_good(node, odb) {
        return Ok(node.entry_count);
    }

    let mut nested_sizes: HashMap<Vec<u8>, i32> = HashMap::new();
    let mut direct_tree_names: HashMap<Vec<u8>, ()> = HashMap::new();
    let mut pos = 0usize;
    while pos < rows.len() {
        let row = rows[pos];
        let path = row.path.as_slice();
        if !entry_matches_prefix(path, anchor, prefix_len) {
            break;
        }

        let tail = path_strip_prefix(path, prefix_len);
        let Some(component_end) = tail.iter().position(|&b| b == b'/') else {
            pos += 1;
            continue;
        };
        let child_name = &tail[..component_end];
        let child = find_or_create_child(node, child_name, true).ok_or_else(|| {
            Error::IndexError(format!(
                "failed to attach cache-tree child '{}'",
                String::from_utf8_lossy(child_name)
            ))
        })?;
        let nested_len = prefix_len + component_end + 1;
        let mut nested_extra = 0i32;
        let covered = rebuild_directory_node(
            odb,
            child,
            &rows[pos..],
            path,
            nested_len,
            &mut nested_extra,
            flags,
        )?;
        if covered <= 0 {
            return Ok(covered);
        }
        nested_sizes.insert(child_name.to_vec(), covered);
        pos += covered as usize;
        *extra_skip += nested_extra;
    }

    node.children.retain(|child| {
        nested_sizes.contains_key(&child.name) || direct_tree_names.contains_key(&child.name)
    });

    let write_opts = WriteOptions {
        silent: flags.silent,
    };

    let mut members: Vec<TreeEntry> = Vec::new();
    let mut stale = false;
    pos = 0;
    while pos < rows.len() {
        let row = rows[pos];
        let path = row.path.as_slice();
        if !entry_matches_prefix(path, anchor, prefix_len) {
            break;
        }

        let tail = path_strip_prefix(path, prefix_len);
        let slash = tail.iter().position(|&b| b == b'/');

        let (oid, mode, name_len, advance) = if let Some(off) = slash {
            let name = &tail[..off];
            let child = find_or_create_child(node, name, false).ok_or_else(|| {
                Error::IndexError(format!(
                    "cache-tree child '{}' missing for '{}'",
                    String::from_utf8_lossy(name),
                    String::from_utf8_lossy(path)
                ))
            })?;
            let covered = *nested_sizes
                .get(name)
                .ok_or_else(|| Error::IndexError("cache-tree nested size missing".to_owned()))?;
            let pending = child.entry_count < 0;
            if pending {
                stale = true;
            }
            let oid = child
                .oid
                .ok_or_else(|| Error::IndexError("cache-tree child oid missing".to_owned()))?;
            if pending && is_empty_tree_oid(odb, &oid) {
                pos += covered as usize;
                continue;
            }
            (oid, MODE_TREE, off, covered as usize)
        } else {
            if row.intent_to_add() {
                stale = true;
                pos += 1;
                continue;
            }
            let mode = if row.is_sparse_directory_placeholder() {
                let name = tail.to_vec();
                direct_tree_names.insert(name.clone(), ());
                if let Some(child) = find_or_create_child(node, &name, true) {
                    child.entry_count = 1;
                    child.oid = Some(row.oid);
                    child.children.clear();
                }
                MODE_TREE
            } else {
                canonicalize_blob_mode(row.mode)
            };
            (row.oid, mode, tail.len(), 1)
        };

        let gitlink = mode == MODE_GITLINK;
        let allow_missing = gitlink || flags.missing_ok || (slash.is_none() && row.intent_to_add());
        if oid.is_zero() || (!allow_missing && !odb.exists(&oid)) {
            return Err(Error::ObjectNotFound(format!(
                "{mode:o} {} at {}",
                oid.to_hex(),
                String::from_utf8_lossy(path)
            )));
        }

        members.push(TreeEntry {
            mode,
            name: tail[..name_len].to_vec(),
            oid,
        });
        pos += advance;
    }

    members.sort_by(|a, b| {
        let a_tree = a.mode == MODE_TREE;
        let b_tree = b.mode == MODE_TREE;
        tree_entry_cmp(&a.name, a_tree, &b.name, b_tree)
    });

    let payload = serialize_tree(&members);
    let (tree_oid, repair_stale) = store_tree_payload(odb, &payload, flags, write_opts)?;
    stale |= repair_stale;

    node.oid = Some(tree_oid);
    node.entry_count = if stale { -1 } else { pos as i32 - *extra_skip };
    Ok(pos as i32)
}

fn cached_node_still_good(node: &CacheTreeNode, odb: &Odb) -> bool {
    cache_tree_fully_valid(odb, Some(node))
}

fn entry_matches_prefix(path: &[u8], anchor: &[u8], prefix_len: usize) -> bool {
    path.len() >= prefix_len && path[..prefix_len] == anchor[..prefix_len]
}

fn path_strip_prefix(path: &[u8], prefix_len: usize) -> &[u8] {
    path[prefix_len..]
        .strip_prefix(b"/")
        .unwrap_or(&path[prefix_len..])
}

#[cfg(test)]
mod test_tree_write_counter {
    use std::sync::atomic::{AtomicUsize, Ordering};

    static TREE_WRITES: AtomicUsize = AtomicUsize::new(0);

    pub fn reset() {
        TREE_WRITES.store(0, Ordering::SeqCst);
    }

    pub fn tree_writes() -> usize {
        TREE_WRITES.load(Ordering::SeqCst)
    }

    pub fn record_tree_write() {
        TREE_WRITES.fetch_add(1, Ordering::SeqCst);
    }
}

fn store_tree_payload(
    odb: &Odb,
    payload: &[u8],
    flags: WriteTreeFlags,
    write_opts: WriteOptions,
) -> Result<(ObjectId, bool)> {
    let hashed = odb.hash(ObjectKind::Tree, payload);
    match flags.persistence {
        WriteTreePersistence::DryRun => Ok((hashed, false)),
        WriteTreePersistence::Repair => Ok((hashed, !odb.exists(&hashed))),
        WriteTreePersistence::Write => {
            #[cfg(test)]
            test_tree_write_counter::record_tree_write();
            Ok((
                odb.write_with_options(ObjectKind::Tree, payload, write_opts)?,
                false,
            ))
        }
    }
}

/// Returns whether `oid` is Git's canonical empty tree (including the legacy hash).
#[must_use]
pub fn is_empty_tree_oid(odb: &Odb, oid: &ObjectId) -> bool {
    if *oid == odb.hash(ObjectKind::Tree, b"") {
        return true;
    }
    // Legacy SHA-1 empty-tree OIDs can appear in cache-trees on SHA-1 repositories.
    if odb.hash_algo() == crate::objects::HashAlgo::Sha1 {
        let hex = oid.to_hex();
        const EMPTY_TREE_CANON: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";
        const EMPTY_TREE_LEGACY: &str = "4b825dc642cb6eb9a060e54bf899d69f7c6948d4";
        if hex == EMPTY_TREE_CANON || hex == EMPTY_TREE_LEGACY {
            return true;
        }
    }
    false
}

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
///
/// Unlike [`write_tree_update_index`], this does not read or update the index cache-tree
/// extension. Callers that need incremental reuse must use [`write_tree_update_index`] on a
/// mutable index instead.
pub fn write_tree_from_index(odb: &Odb, index: &Index, prefix: &str) -> Result<ObjectId> {
    if prefix.is_empty() {
        if let Some(root) = index.cache_tree_root {
            if cache_tree_fully_valid(odb, index.cache_tree.as_ref()) {
                return Ok(root);
            }
        }
    }

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
    build_cache_tree_node(odb, b"", Vec::new(), &entries)
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
    odb.write(ObjectKind::Tree, &data)
}

fn build_cache_tree_node(
    odb: &Odb,
    dir_prefix: &[u8],
    name: Vec<u8>,
    entries: &[&IndexEntry],
) -> Result<CacheTreeNode> {
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
    let mut cache_children = Vec::new();
    for (child_name, child) in children {
        match child {
            ChildKind::Blob { mode, oid } => tree_entries.push(TreeEntry {
                mode,
                name: child_name,
                oid,
            }),
            ChildKind::Tree(sub_prefix, sub_entries) => {
                let child_node =
                    build_cache_tree_node(odb, &sub_prefix, child_name.clone(), &sub_entries)?;
                let oid = child_node.oid.ok_or_else(|| {
                    crate::error::Error::IndexError("cache-tree child missing oid".to_owned())
                })?;
                tree_entries.push(TreeEntry {
                    mode: MODE_TREE,
                    name: child_name,
                    oid,
                });
                cache_children.push(child_node);
            }
        }
    }

    tree_entries.sort_by(|a, b| {
        let a_tree = a.mode == MODE_TREE;
        let b_tree = b.mode == MODE_TREE;
        tree_entry_cmp(&a.name, a_tree, &b.name, b_tree)
    });
    cache_children.sort_by(|a, b| a.name.cmp(&b.name));

    let data = serialize_tree(&tree_entries);
    let oid = odb.write(ObjectKind::Tree, &data)?;
    Ok(CacheTreeNode::valid(
        name,
        entries.len() as i32,
        oid,
        cache_children,
    ))
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
        odb.write(ObjectKind::Tree, &data)
    }

    merge_level(odb, index, base_tree_oid, b"", paths_from_index)
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

    fn sparse_dir_placeholder(path: &str, tree_oid: ObjectId) -> IndexEntry {
        let mut e = entry(path, MODE_TREE, tree_oid);
        e.set_skip_worktree(true);
        e
    }

    #[test]
    fn cache_tree_fully_valid_requires_positive_counts_and_objects() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"x").unwrap();
        let tree_oid = write_tree_from_index(
            &odb,
            &Index {
                entries: vec![entry("x", MODE_REGULAR, blob)],
                ..Index::new()
            },
            "",
        )
        .unwrap();

        assert!(!cache_tree_fully_valid(
            &odb,
            Some(&empty_cache_tree_root())
        ));
        let missing_oid = CacheTreeNode::valid(b"root".to_vec(), 1, tree_oid, vec![]);
        std::fs::remove_file(odb.object_path(&tree_oid)).unwrap();
        assert!(!cache_tree_fully_valid(&odb, Some(&missing_oid)));
    }

    #[test]
    fn write_tree_update_index_reuses_valid_cache_tree() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"same").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("same", MODE_REGULAR, blob));
        let first =
            write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
        let objects_before = std::fs::read_dir(dir.path()).unwrap().count();
        let second =
            write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
        assert_eq!(first, second);
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));
        assert_eq!(
            std::fs::read_dir(dir.path()).unwrap().count(),
            objects_before
        );
    }

    #[test]
    fn cache_tree_update_dry_run_hashes_without_writing_trees() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"d").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("d", MODE_REGULAR, blob));
        let flags = WriteTreeFlags {
            persistence: WriteTreePersistence::DryRun,
            ..WriteTreeFlags::default()
        };
        cache_tree_update(&odb, &mut index, flags).unwrap();
        let root_oid = index.cache_tree.as_ref().unwrap().oid.unwrap();
        assert!(!odb.exists(&root_oid));
        assert_eq!(root_oid, write_tree_from_index(&odb, &index, "").unwrap());
    }

    #[test]
    fn cache_tree_update_repair_reuses_existing_tree_bytes() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"r").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("r", MODE_REGULAR, blob));
        let expected = write_tree_from_index(&odb, &index, "").unwrap();
        index.clear_cache_tree();
        let flags = WriteTreeFlags {
            persistence: WriteTreePersistence::Repair,
            ..WriteTreeFlags::default()
        };
        cache_tree_update(&odb, &mut index, flags).unwrap();
        assert_eq!(index.cache_tree.as_ref().unwrap().oid, Some(expected));
    }

    #[test]
    fn cache_tree_update_rejects_unmerged_index() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"u").unwrap();
        let mut conflict = entry("conflict", MODE_REGULAR, blob);
        conflict.set_stage(1);
        let mut index = Index::new();
        index.add_or_replace(conflict);
        let err = cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap_err();
        assert!(matches!(err, Error::IndexUnmerged));
    }

    #[test]
    fn cache_tree_update_rejects_path_prefix_conflict() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let a = odb.write(ObjectKind::Blob, b"a").unwrap();
        let b = odb.write(ObjectKind::Blob, b"b").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("dir", MODE_REGULAR, a));
        index.add_or_replace(entry("dir/file", MODE_REGULAR, b));
        let err = cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap_err();
        assert!(matches!(err, Error::IndexPathPrefixConflict { .. }));
    }

    #[test]
    fn cache_tree_update_reports_missing_blob() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let missing = ObjectId::from_hex("0000000000000000000000000000000000000001").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("gone", MODE_REGULAR, missing));
        let err = cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap_err();
        assert!(matches!(err, Error::ObjectNotFound(_)));
    }

    #[test]
    fn cache_tree_update_rejects_null_oid_even_with_missing_ok() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"ok").unwrap();
        let zero = ObjectId::zero();
        let mut index = Index::new();
        index.add_or_replace(entry("sub", MODE_GITLINK, zero));
        index.add_or_replace(entry("ok", MODE_REGULAR, blob));
        let err = cache_tree_update(
            &odb,
            &mut index,
            WriteTreeFlags {
                missing_ok: true,
                ..WriteTreeFlags::default()
            },
        )
        .unwrap_err();
        assert!(matches!(err, Error::ObjectNotFound(_)));
    }

    #[test]
    fn cache_tree_update_missing_ok_skips_existence_for_non_null_gitlink() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"ok").unwrap();
        let missing = ObjectId::from_hex("00000000000000000000000000000000000000ab").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("sub", MODE_GITLINK, missing));
        index.add_or_replace(entry("ok", MODE_REGULAR, blob));
        cache_tree_update(
            &odb,
            &mut index,
            WriteTreeFlags {
                missing_ok: true,
                ..WriteTreeFlags::default()
            },
        )
        .unwrap();
        assert!(index.cache_tree.as_ref().unwrap().oid.is_some());
    }

    #[test]
    fn cache_tree_update_ignore_cache_tree_rebuilds_from_scratch() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"i").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("i", MODE_REGULAR, blob));
        index.set_cache_tree(build_cache_tree_from_index(&odb, &index).unwrap());
        index.cache_tree.as_mut().unwrap().entry_count = -1;
        cache_tree_update(
            &odb,
            &mut index,
            WriteTreeFlags {
                ignore_cache_tree: true,
                ..WriteTreeFlags::default()
            },
        )
        .unwrap();
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));
    }

    #[test]
    fn cache_tree_update_marks_ita_entries_stale() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let blob = odb.write(ObjectKind::Blob, b"old").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("old", MODE_REGULAR, blob));
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));

        let mut ita = entry("new", MODE_REGULAR, ObjectId::zero());
        ita.set_intent_to_add(true);
        index.add_or_replace(ita);
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        assert_eq!(index.cache_tree.as_ref().unwrap().entry_count, -1);
    }

    #[test]
    fn sparse_placeholder_survives_ancestor_invalidation() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());

        let inner_blob = odb.write(ObjectKind::Blob, b"inner").unwrap();
        let mut inner_index = Index::new();
        inner_index.add_or_replace(entry("leaf", MODE_REGULAR, inner_blob));
        let inner_tree = write_tree_from_index(&odb, &inner_index, "").unwrap();

        let top_blob = odb.write(ObjectKind::Blob, b"top").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("top", MODE_REGULAR, top_blob));
        index.add_or_replace(sparse_dir_placeholder("wide/sub", inner_tree));
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));

        index.invalidate_cache_tree_for_path(b"wide");
        assert_eq!(index.cache_tree.as_ref().unwrap().entry_count, -1);
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();

        let wide = index
            .cache_tree
            .as_ref()
            .unwrap()
            .children
            .iter()
            .find(|c| c.name == b"wide")
            .expect("wide subtree");
        let sub = wide
            .children
            .iter()
            .find(|c| c.name == b"sub")
            .expect("sparse sub placeholder");
        assert_eq!(sub.oid, Some(inner_tree));
        assert_eq!(sub.entry_count, 1);
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));
    }

    #[test]
    fn rebuilds_cache_tree_after_path_invalidation() {
        let temp_dir = TempDir::new().unwrap();
        let odb = Odb::new(temp_dir.path());

        let oid_f = odb.write(ObjectKind::Blob, b"f").unwrap();
        let oid_deep = odb.write(ObjectKind::Blob, b"deep").unwrap();

        let mut index = Index::new();
        index.add_or_replace(entry("f", MODE_REGULAR, oid_f));
        let root_tree = write_tree_from_index(&odb, &index, "").unwrap();
        index.set_cache_tree(build_cache_tree_from_index(&odb, &index).unwrap());
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));

        index.invalidate_cache_tree_for_path(b"deep");
        index.add_or_replace(entry("deep/very-long-subdir/file", MODE_REGULAR, oid_deep));

        let oid = write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
        assert_ne!(oid, root_tree);
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));
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

        write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();

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

        let root_oid =
            write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
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

        index.clear_cache_tree();
        write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();

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

    fn cache_tree_child_oid(index: &Index, name: &[u8]) -> ObjectId {
        index
            .cache_tree
            .as_ref()
            .unwrap()
            .children
            .iter()
            .find(|c| c.name == name)
            .and_then(|c| c.oid)
            .unwrap()
    }

    fn build_nested_index(odb: &Odb) -> Index {
        let blobs: Vec<ObjectId> = (0..6)
            .map(|i| {
                odb.write(ObjectKind::Blob, format!("blob{i}").as_bytes())
                    .unwrap()
            })
            .collect();
        let mut index = Index::new();
        index.add_or_replace(entry("alpha/one", MODE_REGULAR, blobs[0]));
        index.add_or_replace(entry("alpha/two", MODE_REGULAR, blobs[1]));
        index.add_or_replace(entry("beta/three", MODE_REGULAR, blobs[2]));
        index.add_or_replace(entry("beta/four/deep", MODE_REGULAR, blobs[3]));
        index.add_or_replace(entry("gamma", MODE_REGULAR, blobs[4]));
        index.add_or_replace(entry("wide/sub/leaf", MODE_REGULAR, blobs[5]));
        index
    }

    #[test]
    fn cache_tree_update_matches_full_rebuild() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let mut index = build_nested_index(&odb);
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        let incremental_root = index.cache_tree.as_ref().unwrap().oid.unwrap();

        let mut seed = 0xDEADBEEF_u32;
        for step in 0..12 {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            let path: &[u8] = match step % 5 {
                0 => b"alpha",
                1 => b"beta",
                2 => b"beta/four",
                3 => b"wide",
                _ => b"gamma",
            };
            index.invalidate_cache_tree_for_path(path);
            if step % 3 == 0 {
                let blob = odb
                    .write(ObjectKind::Blob, format!("mut{step}").as_bytes())
                    .unwrap();
                let extra_path = format!("alpha/extra{step}");
                index.add_or_replace(entry(&extra_path, MODE_REGULAR, blob));
            }
            cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
            let full = build_cache_tree_from_index(&odb, &index).unwrap();
            assert_eq!(
                index.cache_tree.as_ref().unwrap().oid,
                full.oid,
                "step {step}: incremental cache-tree must match full rebuild"
            );
        }
        let _ = incremental_root;
    }

    #[test]
    fn cache_tree_update_rewrites_only_invalidated() {
        use filetime::FileTime;
        use std::fs;

        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let mut index = build_nested_index(&odb);
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        let alpha_before = cache_tree_child_oid(&index, b"alpha");
        let wide_before = cache_tree_child_oid(&index, b"wide");
        let beta_before = cache_tree_child_oid(&index, b"beta");
        let alpha_path = odb.object_path(&alpha_before);
        let wide_path = odb.object_path(&wide_before);
        let alpha_mtime =
            FileTime::from_last_modification_time(&fs::metadata(&alpha_path).unwrap());
        let wide_mtime = FileTime::from_last_modification_time(&fs::metadata(&wide_path).unwrap());

        index.invalidate_cache_tree_for_path(b"beta/four");
        let blob = odb.write(ObjectKind::Blob, b"updated").unwrap();
        index.add_or_replace(entry("beta/four/deep", MODE_REGULAR, blob));

        test_tree_write_counter::reset();
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        let tree_writes = test_tree_write_counter::tree_writes();
        assert!(
            (2..=3).contains(&tree_writes),
            "expected root + beta subtree tree writes, got {tree_writes}"
        );
        assert_eq!(cache_tree_child_oid(&index, b"alpha"), alpha_before);
        assert_eq!(cache_tree_child_oid(&index, b"wide"), wide_before);
        assert_ne!(cache_tree_child_oid(&index, b"beta"), beta_before);
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(&alpha_path).unwrap()),
            alpha_mtime,
            "unchanged alpha tree must not be rewritten"
        );
        assert_eq!(
            FileTime::from_last_modification_time(&fs::metadata(&wide_path).unwrap()),
            wide_mtime,
            "unchanged wide tree must not be rewritten"
        );
    }

    #[test]
    fn cache_tree_update_rebuilds_missing_tree() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let mut index = build_nested_index(&odb);
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        let beta_oid = cache_tree_child_oid(&index, b"beta");
        let root_oid = index.cache_tree.as_ref().unwrap().oid.unwrap();
        std::fs::remove_file(odb.object_path(&beta_oid)).unwrap();
        assert!(!odb.exists(&beta_oid));
        assert!(
            odb.exists(&root_oid),
            "root tree remains while child is missing"
        );

        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        assert!(odb.exists(&cache_tree_child_oid(&index, b"beta")));
        assert!(cache_tree_fully_valid(&odb, index.cache_tree.as_ref()));
    }

    #[test]
    fn write_tree_update_recreates_missing_child_with_valid_root() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let mut index = Index::new();
        let blob = odb.write(ObjectKind::Blob, b"x").unwrap();
        index.add_or_replace(entry("dir/x", MODE_REGULAR, blob));
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();
        let dir_oid = cache_tree_child_oid(&index, b"dir");
        std::fs::remove_file(odb.object_path(&dir_oid)).unwrap();

        write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
        assert!(odb.exists(&cache_tree_child_oid(&index, b"dir")));
    }

    fn sha256_odb(temp: &TempDir) -> Odb {
        let git_dir = temp.path().join(".git");
        std::fs::create_dir_all(git_dir.join("objects")).unwrap();
        std::fs::write(
            git_dir.join("config"),
            "[core]\n\trepositoryformatversion = 1\n[extensions]\n\tobjectformat = sha256\n",
        )
        .unwrap();
        Odb::new(&git_dir.join("objects")).with_config_git_dir(git_dir)
    }

    #[test]
    fn nested_intent_to_add_omits_empty_directory_sha256() {
        let temp = TempDir::new().unwrap();
        let odb = sha256_odb(&temp);
        assert_eq!(odb.hash_algo(), crate::objects::HashAlgo::Sha256);

        let keep = odb.write(ObjectKind::Blob, b"keep").unwrap();
        let mut index = Index::new();
        index.add_or_replace(entry("keep", MODE_REGULAR, keep));
        let mut ita = entry("ita/new", MODE_REGULAR, ObjectId::zero());
        ita.set_intent_to_add(true);
        index.add_or_replace(ita);

        let root =
            write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
        let entries = parse_tree(&odb.read(&root).unwrap().data).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, b"keep");
    }

    #[test]
    fn cache_tree_update_prefix() {
        let dir = TempDir::new().unwrap();
        let odb = Odb::new(dir.path());
        let mut index = build_nested_index(&odb);
        cache_tree_update(&odb, &mut index, WriteTreeFlags::default()).unwrap();

        let full =
            write_tree_update_index(&odb, &mut index, "", WriteTreeFlags::default()).unwrap();
        let sub =
            write_tree_update_index(&odb, &mut index, "beta", WriteTreeFlags::default()).unwrap();
        assert_ne!(full, sub);

        let beta = index
            .cache_tree
            .as_ref()
            .unwrap()
            .children
            .iter()
            .find(|c| c.name == b"beta")
            .expect("beta cache-tree child");
        assert_eq!(beta.oid, Some(sub));
    }
}
