//! `git merge` index/head algorithm core.
//!
//! The full merge command in the `grit` binary is a large stateful sequencer:
//! it parses args, launches the merge-message editor, runs hooks, prints
//! progress (`"Trying merge strategy ..."`, `"Already up to date."`), and
//! writes the working tree. Those responsibilities — argv parsing, terminal
//! output, editor/hook subprocess dispatch, and exit-code mapping — stay in the
//! CLI.
//!
//! What lives here is the self-contained, presentation-free part of that
//! sequencer: the pure data transforms over trees, indexes, and the commit
//! graph that the CLI calls but that compute results from repository data
//! alone.
//!
//! # What this module owns
//!
//! - [`reduce_octopus_merge_heads`] — Git's "reduce parents": drop any merge
//!   head that is an ancestor of another listed head, preserving input order.
//! - [`compose_fast_forward_index`] — build the post-fast-forward index from the
//!   target tree, carrying forward staged additions that the fast-forward does
//!   not touch.
//! - [`compose_octopus_final_index`] — fold staged paths from before an octopus
//!   merge back into the merge result.
//! - [`tree_to_index_entries`] / [`tree_to_map`] — flatten a tree object into a
//!   recursive list (or path-keyed map) of [`IndexEntry`] values; the shared
//!   building block the three transforms above (and most of the CLI merge code)
//!   are written against.

use std::collections::{BTreeSet, HashMap};

use crate::error::{Error, Result};
use crate::index::{Index, IndexEntry};
use crate::merge_base::is_ancestor;
use crate::objects::{parse_tree, ObjectId, ObjectKind};
use crate::repo::Repository;

/// Flatten a tree object into a recursive list of [`IndexEntry`] values.
///
/// Sub-trees are walked depth-first and their paths are prefixed with `prefix`
/// joined by `/`. The returned entries carry stage 0, zeroed stat fields, and
/// the tree's mode/oid — exactly what a freshly read-tree index holds.
///
/// # Errors
///
/// Returns an error if `oid` cannot be read or does not name a tree object.
pub fn tree_to_index_entries(
    repo: &Repository,
    oid: &ObjectId,
    prefix: &str,
) -> Result<Vec<IndexEntry>> {
    let obj = repo.odb.read(oid)?;
    if obj.kind != ObjectKind::Tree {
        return Err(Error::Message(format!("expected tree, got {}", obj.kind)));
    }
    let entries = parse_tree(&obj.data)?;
    let mut result = Vec::new();

    for te in entries {
        let name = String::from_utf8_lossy(&te.name).into_owned();
        let path = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}/{name}")
        };

        if te.mode == 0o040000 {
            let sub = tree_to_index_entries(repo, &te.oid, &path)?;
            result.extend(sub);
        } else {
            let path_bytes = path.into_bytes();
            result.push(IndexEntry {
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
                flags: path_bytes.len().min(0xFFF) as u16,
                flags_extended: None,
                path: path_bytes,
                base_index_pos: 0,
            });
        }
    }
    Ok(result)
}

/// Index a flat entry list by path, keeping the last entry for any duplicate.
#[must_use]
pub fn tree_to_map(entries: Vec<IndexEntry>) -> HashMap<Vec<u8>, IndexEntry> {
    let mut out = HashMap::new();
    for e in entries {
        out.insert(e.path.clone(), e);
    }
    out
}

/// Build the index that results from fast-forwarding `current_index` to
/// `target_tree`.
///
/// Starts from `target_tree`'s flattened entries and carries forward any
/// stage-0 path that is staged in `current_index`, is absent from
/// `target_tree`, and is also absent from `head_tree` — i.e. a staged addition
/// that the fast-forward should not discard.
///
/// # Errors
///
/// Returns an error if `target_tree` or `head_tree` cannot be read as trees.
pub fn compose_fast_forward_index(
    repo: &Repository,
    target_tree: ObjectId,
    head_tree: ObjectId,
    current_index: &Index,
) -> Result<Index> {
    let mut new_entries = tree_to_index_entries(repo, &target_tree, "")?;
    let target_paths: BTreeSet<Vec<u8>> = new_entries.iter().map(|e| e.path.clone()).collect();
    let head_entries = tree_to_map(tree_to_index_entries(repo, &head_tree, "")?);
    for e in &current_index.entries {
        if e.stage() != 0 {
            continue;
        }
        if target_paths.contains(&e.path) {
            continue;
        }
        // Staged addition: not in HEAD — keep alongside the fast-forwarded tree.
        if !head_entries.contains_key(&e.path) {
            new_entries.push(e.clone());
        }
    }
    let mut index = Index::new();
    index.entries = new_entries;
    index.sort();
    // A duplicate-entry tree (t4058) flattens to several identical-path entries; Git's index keeps
    // only one per path. Restore that invariant so status/diff against HEAD is consistent.
    index.dedup_paths_keep_last();
    Ok(index)
}

/// Preserve staged paths from before an octopus merge that the merge result does not touch
/// (e.g. unrelated `git add`), matching Git's index composition.
pub fn compose_octopus_final_index(pre_merge_index: &Index, final_index: &mut Index) {
    let final_paths: BTreeSet<Vec<u8>> = final_index
        .entries
        .iter()
        .filter(|e| e.stage() == 0)
        .map(|e| e.path.clone())
        .collect();
    for e in &pre_merge_index.entries {
        if e.stage() != 0 {
            continue;
        }
        if final_paths.contains(&e.path) {
            continue;
        }
        final_index.push_entry_unsorted(e.clone());
    }
    final_index.sort();
}

/// Drop merge heads that are ancestors of another listed head (Git's "reduce parents").
///
/// Order of the remaining heads matches the first occurrence in the input (t7603).
///
/// # Errors
///
/// Returns an error if a commit-graph walk required for an ancestry test fails.
pub fn reduce_octopus_merge_heads(
    repo: &Repository,
    merge_oids: &[ObjectId],
    merge_names: &[String],
) -> Result<(Vec<ObjectId>, Vec<String>)> {
    debug_assert_eq!(merge_oids.len(), merge_names.len());
    let mut out_oids = Vec::with_capacity(merge_oids.len());
    let mut out_names = Vec::with_capacity(merge_names.len());
    for i in 0..merge_oids.len() {
        let oid = merge_oids[i];
        let redundant = merge_oids
            .iter()
            .enumerate()
            .any(|(j, &other)| j != i && is_ancestor(repo, oid, other).unwrap_or(false));
        if !redundant {
            out_oids.push(oid);
            out_names.push(merge_names[i].clone());
        }
    }
    Ok((out_oids, out_names))
}
