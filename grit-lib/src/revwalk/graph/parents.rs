//! Display parent rewriting for graph walks (skip filtered commits).

use std::collections::{HashMap, HashSet};

use crate::error::Result;
use crate::objects::{parse_commit, read_commit_subject_line, ObjectId, ObjectKind};
use crate::repo::Repository;
use crate::rev_list::CommitGraph;

/// Parents and oneline subjects keyed by commit id.
pub type LogGraphMaterialized = (HashMap<ObjectId, Vec<ObjectId>>, HashMap<ObjectId, String>);

/// Parent lists for graph layout: direct ODB parents, or rewritten when filters hide commits.
///
/// When `rewrite_through_hidden` is false (plain `log --graph` without path or commit filters),
/// parents are read once through the commit-graph cache.
///
/// # Errors
///
/// Propagates object read and parse errors.
pub fn display_parents_for_graph(
    repo: &Repository,
    visible: &HashSet<ObjectId>,
    rewrite_through_hidden: bool,
) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
    let oids: Vec<ObjectId> = visible.iter().copied().collect();
    log_graph_materialize(repo, &oids, rewrite_through_hidden).map(|(parents, _)| parents)
}

/// Parents and oneline subjects for graph rendering in one commit-graph pass.
///
/// # Errors
///
/// Propagates object read and parse errors.
pub fn log_graph_materialize(
    repo: &Repository,
    oids: &[ObjectId],
    rewrite_through_hidden: bool,
) -> Result<LogGraphMaterialized> {
    if rewrite_through_hidden {
        let visible: HashSet<ObjectId> = oids.iter().copied().collect();
        let parents = rewrite_display_parents_map(repo, &visible)?;
        let mut subjects = HashMap::with_capacity(oids.len());
        for &oid in oids {
            subjects.insert(oid, read_subject_only(repo, oid)?);
        }
        return Ok((parents, subjects));
    }
    let mut graph = CommitGraph::with_commit_graph(repo, false, true);
    let mut parents = HashMap::with_capacity(oids.len());
    let mut subjects = HashMap::with_capacity(oids.len());
    for &oid in oids {
        parents.insert(oid, graph.parents_of(oid)?);
        let subject = match graph.subject_of(oid) {
            Some(s) => s.to_owned(),
            None => read_subject_only(repo, oid)?,
        };
        subjects.insert(oid, subject);
    }
    Ok((parents, subjects))
}

fn read_subject_only(repo: &Repository, oid: ObjectId) -> Result<String> {
    let object = repo.odb().read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(crate::error::Error::CorruptObject(format!(
            "expected commit object, got {:?} for {oid}",
            object.kind
        )));
    }
    read_commit_subject_line(&object.data)
}

/// Rewrite parents for every commit in `visible`, skipping filtered-out ancestors.
///
/// Hidden merges keep connectivity: every visible ancestor reachable through any
/// parent edge of a hidden commit is retained.
///
/// # Errors
///
/// Propagates object read and parse errors while walking parents.
pub fn rewrite_display_parents_map(
    repo: &Repository,
    visible: &HashSet<ObjectId>,
) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
    let mut raw_parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    let mut out = HashMap::new();
    for &oid in visible {
        let parents = load_parents(repo, oid, &mut raw_parents)?;
        let mut rewritten = Vec::new();
        for &parent in &parents {
            for visible_ancestor in visible_through_hidden(repo, parent, visible, &mut raw_parents)?
            {
                if !rewritten.contains(&visible_ancestor) {
                    rewritten.push(visible_ancestor);
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

/// Visible commits reachable from `start` by walking through non-visible commits only.
fn visible_through_hidden(
    repo: &Repository,
    start: ObjectId,
    visible: &HashSet<ObjectId>,
    raw_parents: &mut HashMap<ObjectId, Vec<ObjectId>>,
) -> Result<Vec<ObjectId>> {
    if visible.contains(&start) {
        return Ok(vec![start]);
    }
    let mut stack = vec![start];
    let mut seen = HashSet::new();
    let mut found = Vec::new();
    while let Some(cur) = stack.pop() {
        if !seen.insert(cur) {
            continue;
        }
        if visible.contains(&cur) {
            if !found.contains(&cur) {
                found.push(cur);
            }
            continue;
        }
        for parent in load_parents(repo, cur, raw_parents)? {
            stack.push(parent);
        }
    }
    Ok(found)
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
