//! Display parent rewriting for graph walks (skip filtered commits).

use std::collections::{HashMap, HashSet};

use crate::error::Result;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::repo::Repository;

/// Rewrite parents for every commit in `visible`, skipping filtered-out ancestors.
///
/// # Errors
///
/// Propagates object read and parse errors while walking parents.
pub fn rewrite_display_parents_map(
    repo: &Repository,
    visible: &HashSet<ObjectId>,
) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
    let mut raw_parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut nearest_cache: HashMap<ObjectId, Option<ObjectId>> = HashMap::new();
    let mut out = HashMap::new();
    for &oid in visible {
        let parents = load_parents(repo, oid, &mut raw_parents)?;
        let mut rewritten = Vec::new();
        for &parent in &parents {
            if let Some(rewritten_parent) =
                nearest_visible_cached(repo, parent, visible, &mut raw_parents, &mut nearest_cache)?
            {
                if !rewritten.contains(&rewritten_parent) {
                    rewritten.push(rewritten_parent);
                }
            }
        }
        out.insert(oid, rewritten);
    }
    Ok(out)
}

/// Rewrite `oid`'s parents to the nearest ancestors present in `visible`.
///
/// # Errors
///
/// Propagates object read and parse errors while walking parents.
pub fn rewrite_display_parents(
    repo: &Repository,
    oid: ObjectId,
    visible: &HashSet<ObjectId>,
) -> Result<Vec<ObjectId>> {
    rewrite_display_parents_map(repo, visible).map(|map| map.get(&oid).cloned().unwrap_or_default())
}

fn nearest_visible_cached(
    repo: &Repository,
    start: ObjectId,
    visible: &HashSet<ObjectId>,
    raw_parents: &mut HashMap<ObjectId, Vec<ObjectId>>,
    cache: &mut HashMap<ObjectId, Option<ObjectId>>,
) -> Result<Option<ObjectId>> {
    if let Some(hit) = cache.get(&start) {
        return Ok(*hit);
    }
    let mut current = start;
    let mut seen = HashSet::new();
    let resolved = loop {
        if visible.contains(&current) {
            break Some(current);
        }
        if !seen.insert(current) {
            break None;
        }
        let parents = load_parents(repo, current, raw_parents)?;
        let Some(&parent) = parents.first() else {
            break None;
        };
        if let Some(hit) = cache.get(&parent) {
            break *hit;
        }
        current = parent;
    };
    cache.insert(start, resolved);
    Ok(resolved)
}

fn load_parents(
    repo: &Repository,
    oid: ObjectId,
    cache: &mut HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    if let Some(parents) = cache.get(&oid) {
        return Ok(parents.clone());
    }
    let commit = read_commit(repo, oid)?;
    cache.insert(oid, commit.parents.clone());
    Ok(commit.parents)
}

fn read_commit(repo: &Repository, oid: ObjectId) -> Result<crate::objects::CommitData> {
    let object = repo.odb().read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(crate::error::Error::CorruptObject(format!(
            "expected commit object, got {:?} for {oid}",
            object.kind
        )));
    }
    parse_commit(&object.data)
}
