//! Display parent rewriting for graph walks (skip filtered commits).

use std::collections::{HashMap, HashSet};

use crate::error::Result;
use crate::objects::{read_commit_subject_line, ObjectId};
use crate::repo::Repository;
use crate::rev_list::CommitGraph;
use crate::revwalk::RevWalk;

/// Parents and oneline subjects keyed by commit id.
pub type LogGraphMaterialized = (HashMap<ObjectId, Vec<ObjectId>>, HashMap<ObjectId, String>);

impl RevWalk<'_> {
    /// Parents and oneline subjects of `oids` for `log --graph`, in one commit-graph pass.
    ///
    /// With `rewrite_through_hidden` (a path or commit filter hides some commits), each
    /// commit's parents are rewritten to its nearest ancestors among `oids`, as
    /// [`Self::rewrite_display_parents`] does; otherwise they are the commits' own parents.
    ///
    /// # Errors
    ///
    /// Propagates object read and parse errors.
    pub fn graph_materialize(
        &self,
        oids: &[ObjectId],
        rewrite_through_hidden: bool,
    ) -> Result<LogGraphMaterialized> {
        let repo = self.repository();
        if rewrite_through_hidden {
            let visible: HashSet<ObjectId> = oids.iter().copied().collect();
            let parents = self.rewrite_display_parents(&visible)?;
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

    /// Rewrite the parents of every commit in `visible` to its nearest ancestors in
    /// `visible`, skipping filtered-out commits.
    ///
    /// Hidden merges keep connectivity: every visible ancestor reachable through any
    /// parent edge of a hidden commit is retained.
    ///
    /// # Errors
    ///
    /// Propagates object read and parse errors while walking parents.
    pub fn rewrite_display_parents(
        &self,
        visible: &HashSet<ObjectId>,
    ) -> Result<HashMap<ObjectId, Vec<ObjectId>>> {
        let repo = self.repository();
        let mut raw_parents: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
        let mut out = HashMap::new();
        for &oid in visible {
            let parents = load_parents(repo, oid, &mut raw_parents)?;
            let mut rewritten = Vec::new();
            for &parent in &parents {
                for visible_ancestor in
                    visible_through_hidden(repo, parent, visible, &mut raw_parents)?
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
}

fn read_subject_only(repo: &Repository, oid: ObjectId) -> Result<String> {
    let bytes = repo.read_commit_bytes_with_replace(&oid)?;
    read_commit_subject_line(&bytes)
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
    let commit = repo.read_commit_with_replace(&oid)?;
    cache.insert(oid, commit.parents.clone());
    Ok(commit.parents)
}
