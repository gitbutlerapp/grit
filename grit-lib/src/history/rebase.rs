//! Stateless rebase onto a new base (`upstream..tip` replayed onto `onto`).

use std::collections::{HashMap, HashSet};

use crate::merge_base::is_ancestor;
use crate::objects::ObjectId;
use crate::patch_ids::{compute_patch_id, PatchIdMode};
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};

use super::error::HistoryError;
use super::plan::{collect_scoped_refs, commit_parents, plan_rewrite_with_order};
use super::types::{CommitEdit, HistoryEdits, RefScope, RewriteOptions, RewritePlan};

/// Options for [`rebase_onto`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseOntoOptions {
    /// Scope, empty-commit policy, and committer ident for replayed commits.
    pub rewrite: RewriteOptions,
    /// When `true` (default), omit commits whose stable patch-id already appears in `onto`'s history.
    pub skip_applied: bool,
    /// When `true`, drop merge commits that are branch tips in the replay range and move refs to
    /// the replayed second parent (matches `git pull --rebase=merges` on linearizable merges).
    pub flatten_leaf_merges: bool,
}

impl RebaseOntoOptions {
    /// Default rebase options: update descendant branches, drop empty replays, skip applied patches.
    #[must_use]
    pub fn new(rewrite: RewriteOptions) -> Self {
        Self {
            rewrite,
            skip_applied: true,
            flatten_leaf_merges: false,
        }
    }
}

/// Plan replay of `upstream..tips` onto `onto` in topological order.
///
/// Merge commits in the range are replayed with updated parents and trees. Under
/// [`RefScope::Branches`](super::RefScope::Branches),
/// every local branch whose tip descends from the rewritten range is moved to its replayed tip.
///
/// # Errors
///
/// Returns [`HistoryError::Conflict`] and other planning
/// failures from [`crate::history::plan_rewrite`].
pub fn rebase_onto(
    repo: &Repository,
    upstream: ObjectId,
    onto: ObjectId,
    tips: &[ObjectId],
    opts: &RebaseOntoOptions,
) -> Result<RewritePlan, HistoryError> {
    if tips.is_empty() {
        return Err(HistoryError::Planning {
            message: "at least one tip commit is required".into(),
        });
    }

    for &tip in tips {
        if !is_ancestor(repo, upstream, tip).map_err(library_err)? {
            return Err(HistoryError::Planning {
                message: format!(
                    "upstream {} is not an ancestor of tip {}",
                    upstream.to_hex(),
                    tip.to_hex()
                ),
            });
        }
    }

    let range = commits_in_rebase_range(repo, upstream, tips)?;
    if range.is_empty() {
        return Ok(RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: super::types::HeadEffect::default(),
        });
    }

    let skip_map = if opts.skip_applied {
        applied_by_patch(repo, onto, &range)?
    } else {
        HashMap::new()
    };

    let range_set: HashSet<ObjectId> = range.iter().copied().collect();
    let mut edits = HistoryEdits::new();
    for &commit in &range {
        if let Some(&existing) = skip_map.get(&commit) {
            edits.insert(commit, CommitEdit::Reuse(existing));
            continue;
        }
        let parents = commit_parents(repo, commit)?;
        let is_local_root = parents
            .iter()
            .all(|parent| *parent == upstream || !range_set.contains(parent));
        if is_local_root {
            let reparent_onto = if let Some(mainline) =
                mainline_anchor_for_parallel_root(repo, commit, &range_set)?
            {
                mainline
            } else {
                onto
            };
            edits.insert(
                commit,
                CommitEdit::Reparent {
                    parent: reparent_onto,
                },
            );
        }
        if opts.flatten_leaf_merges && is_leaf_merge_in_range(repo, commit, &range_set)? {
            edits.insert(commit, CommitEdit::Drop);
        }
    }

    let replay_order =
        extend_rebase_replay_order(repo, upstream, &range, &opts.rewrite, &skip_map)?;
    plan_rewrite_with_order(
        repo,
        &edits,
        &opts.rewrite,
        Some(&replay_order),
        opts.flatten_leaf_merges,
    )
}

/// Include sibling branch tips (same parent in the rebase range) when updating all descendant refs.
fn extend_rebase_replay_order(
    repo: &Repository,
    upstream: ObjectId,
    range: &[ObjectId],
    opts: &RewriteOptions,
    skip_map: &HashMap<ObjectId, ObjectId>,
) -> Result<Vec<ObjectId>, HistoryError> {
    if range.is_empty() {
        return Ok(Vec::new());
    }
    if matches!(opts.scope, RefScope::Head) {
        return Ok(range.to_vec());
    }
    let mut order = range.to_vec();
    let mut in_order: HashSet<ObjectId> = order.iter().copied().collect();
    let oldest = range[0];
    let scoped = collect_scoped_refs(repo, opts, oldest)?;
    let mut changed = true;
    while changed {
        changed = false;
        for (_, tip) in &scoped {
            if in_order.contains(tip) || skip_map.contains_key(tip) {
                continue;
            }
            let parents = commit_parents(repo, *tip)?;
            if parents
                .iter()
                .all(|parent| *parent == upstream || in_order.contains(parent))
            {
                order.push(*tip);
                in_order.insert(*tip);
                changed = true;
            }
        }
    }
    Ok(order)
}

/// When `commit` is the tip of a side branch merged into the range, return the first parent
/// of that merge on the mainline so the side replay chains onto the replayed mainline commit.
fn mainline_anchor_for_parallel_root(
    repo: &Repository,
    commit: ObjectId,
    range_set: &HashSet<ObjectId>,
) -> Result<Option<ObjectId>, HistoryError> {
    for &merge_oid in range_set {
        let parents = commit_parents(repo, merge_oid)?;
        if parents.len() >= 2 && parents[1] == commit {
            return Ok(Some(parents[0]));
        }
    }
    Ok(None)
}

fn is_leaf_merge_in_range(
    repo: &Repository,
    oid: ObjectId,
    range_set: &HashSet<ObjectId>,
) -> Result<bool, HistoryError> {
    let parents = commit_parents(repo, oid)?;
    if parents.len() <= 1 {
        return Ok(false);
    }
    for &other in range_set {
        if other == oid {
            continue;
        }
        let parents = commit_parents(repo, other)?;
        if parents.contains(&oid) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn commits_in_rebase_range(
    repo: &Repository,
    upstream: ObjectId,
    tips: &[ObjectId],
) -> Result<Vec<ObjectId>, HistoryError> {
    let positive: Vec<String> = tips.iter().map(ObjectId::to_hex).collect();
    let negative = vec![upstream.to_hex()];
    let walk_opts = RevListOptions {
        ordering: OrderingMode::Topo,
        reverse: false,
        use_commit_graph: true,
        full_history: true,
        ..RevListOptions::default()
    };
    let walk = rev_list(repo, &positive, &negative, &walk_opts).map_err(library_err)?;
    let mut commits = walk.commits;
    commits.retain(|oid| *oid != upstream);
    for &tip in tips {
        if tip != upstream && !commits.contains(&tip) {
            commits.push(tip);
        }
    }
    // Topological oldest-first (rev-list topo with reverse=false is newest-first).
    commits.reverse();
    Ok(commits)
}

fn patch_ids_in_history(
    repo: &Repository,
    start: ObjectId,
) -> Result<HashMap<ObjectId, ObjectId>, HistoryError> {
    let positive = vec![start.to_hex()];
    let walk_opts = RevListOptions {
        ordering: OrderingMode::Topo,
        reverse: false,
        use_commit_graph: true,
        full_history: true,
        ..RevListOptions::default()
    };
    let walk = rev_list(repo, &positive, &[], &walk_opts).map_err(library_err)?;
    let algo = repo.odb().hash_algo();
    let mut by_patch: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut commits = walk.commits;
    commits.reverse();
    for oid in commits {
        if let Ok(Some(pid)) = compute_patch_id(algo, repo.odb(), &oid, PatchIdMode::Stable) {
            by_patch.insert(pid, oid);
        }
    }
    Ok(by_patch)
}

/// Map each commit in `range` to an existing commit in `onto` history with the same patch-id.
fn applied_by_patch(
    repo: &Repository,
    onto: ObjectId,
    range: &[ObjectId],
) -> Result<HashMap<ObjectId, ObjectId>, HistoryError> {
    let by_patch = patch_ids_in_history(repo, onto)?;
    let algo = repo.odb().hash_algo();
    let mut out = HashMap::new();
    for &commit in range {
        let Some(pid) = compute_patch_id(algo, repo.odb(), &commit, PatchIdMode::Stable)
            .map_err(library_err)?
        else {
            continue;
        };
        if let Some(&existing) = by_patch.get(&pid) {
            out.insert(commit, existing);
        }
    }
    Ok(out)
}

fn library_err(err: crate::error::Error) -> HistoryError {
    if let crate::error::Error::History(inner) = err {
        return inner;
    }
    HistoryError::Library(Box::new(err))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::index::write_tree::write_tree_from_index;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::objects::{serialize_commit, CommitData, ObjectKind};

    fn ident(n: i64) -> String {
        format!("T <t@grit.test> {n} +0000")
    }

    fn blob_tree(repo: &Repository, path: &str, bytes: &[u8]) -> crate::error::Result<ObjectId> {
        let blob = repo.odb().write(ObjectKind::Blob, bytes)?;
        let mut index = Index::new();
        index.hash_algo = repo.odb().hash_algo();
        index.add_or_replace(IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: bytes.len() as u32,
            oid: blob,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        });
        write_tree_from_index(repo.odb(), &index, "")
    }

    fn write_commit(
        repo: &Repository,
        tree: ObjectId,
        parents: &[ObjectId],
        msg: &str,
        id: &str,
    ) -> crate::error::Result<ObjectId> {
        let raw = serialize_commit(&CommitData {
            tree,
            parents: parents.to_vec(),
            author: id.to_owned(),
            committer: id.to_owned(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: format!("{msg}\n"),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        repo.odb().write(ObjectKind::Commit, &raw)
    }

    #[test]
    fn patch_lookup_finds_equivalent_in_onto_history() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let tree_a = blob_tree(&repo, "f.txt", b"a\n").expect("tree");
        let tree_b = blob_tree(&repo, "f.txt", b"b\n").expect("tree");
        let root = write_commit(&repo, tree_a, &[], "root", &id).expect("root");
        let onto_tip = write_commit(&repo, tree_b, &[root], "on base", &id).expect("onto");
        let replay = write_commit(&repo, tree_b, &[root], "on base", &id).expect("replay");
        let range = vec![replay];
        let map = applied_by_patch(&repo, onto_tip, &range).expect("map");
        assert_eq!(map.get(&replay), Some(&onto_tip));
    }
}
