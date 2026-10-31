//! Merge-base and reachability primitives.
//!
//! This module implements the subset needed by `grit merge-base`:
//! default merge-base selection, `--all`, `--octopus`, `--independent`,
//! and `--is-ancestor`.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashSet, VecDeque};

use crate::error::{Error, Result};
use crate::ewah_bitmap::Bitmap;
use crate::log_walk::WalkedCommits;
use crate::objects::{
    oid_hash_map, oid_hash_set, oid_hash_set_from, CommitRef, ObjectId, ObjectKind, OidHashMap,
    OidHashSet,
};
use crate::pack_bitmap::BitmapIndex;
use crate::promisor::{read_promisor_missing_oids, repo_treats_promisor_packs};
use crate::reflog::read_reflog;
use crate::repo::Repository;
use crate::rev_parse::{
    load_graft_parents, peel_to_commit_for_merge_base, resolve_revision,
    resolve_upstream_symbolic_name, try_peel_to_commit_for_merge_base, upstream_suffix_info,
};
use crate::shallow::load_shallow_boundaries;

/// Resolve commit-ish command arguments to commit object IDs.
///
/// # Parameters
///
/// - `repo` - repository used for revision lookup and object reads.
/// - `specs` - revision arguments such as `HEAD`, ref names, or object IDs.
///
/// # Errors
///
/// Returns [`Error::ObjectNotFound`] when a revision does not resolve and
/// [`Error::CorruptObject`] when the resolved object is not a commit.
pub fn resolve_commit_specs(repo: &Repository, specs: &[String]) -> Result<Vec<ObjectId>> {
    let mut out = Vec::with_capacity(specs.len());
    for spec in specs {
        let oid = resolve_revision(repo, spec)?;
        ensure_is_commit(repo, oid)?;
        out.push(oid);
    }
    Ok(out)
}

/// Compute merge bases for one commit vs one or more others.
///
/// Semantics match Git's default mode: for `<a> <b>...`, this computes merge
/// bases between `a` and a hypothetical merge of all remaining commits.
///
/// # Parameters
///
/// - `repo` - repository used to walk commit parents.
/// - `first` - first commit argument.
/// - `others` - remaining commit arguments.
///
/// # Errors
///
/// Returns parse and object read errors from commit traversal.
pub fn merge_bases_first_vs_rest(
    repo: &Repository,
    first: ObjectId,
    others: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    if others.is_empty() {
        return Ok(vec![first]);
    }
    for &other in others {
        if first == other {
            return Ok(vec![first]);
        }
    }
    if others.len() == 1 {
        let other = others[0];
        let mut cache = CommitGraphCache::new(repo);
        match direct_ancestor_relation(&mut cache, first, other)? {
            FirstParentRelation::Same | FirstParentRelation::FirstIsAncestor => {
                return Ok(vec![first]);
            }
            FirstParentRelation::SecondIsAncestor => return Ok(vec![other]),
            FirstParentRelation::Unrelated => {}
        }
        let mut selected = walk_intersecting_histories(&mut cache, first, others, false)?;
        if selected.is_empty() {
            return Ok(Vec::new());
        }
        sort_merge_bases_for_git_output(&mut cache, &mut selected)?;
        if selected.len() > 1 {
            selected.truncate(1);
        }
        return Ok(selected);
    }
    let mut cache = CommitGraphCache::new(repo);
    let first_anc = cache.ancestor_closure(first)?;
    let mut others_union = oid_hash_set();
    for &other in others {
        others_union.extend(cache.ancestor_closure(other)?);
    }
    let candidates: OidHashSet = first_anc.intersection(&others_union).copied().collect();
    let all_minimal = reduce_to_best(candidates, &mut cache)?;
    if all_minimal.len() <= 1 {
        return Ok(all_minimal);
    }

    let mut selected = walk_intersecting_histories(&mut cache, first, others, false)?;
    if selected.is_empty() {
        selected = all_minimal;
    }
    sort_merge_bases_for_git_output(&mut cache, &mut selected)?;
    if selected.len() > 1 {
        selected.truncate(1);
    }
    Ok(selected)
}

/// Merge base of `HEAD` and one other commit, matching `git diff --merge-base <commit>`.
///
/// Returns an error when there is no merge base or more than one.
pub fn merge_base_for_diff_index(
    repo: &Repository,
    head: ObjectId,
    other: ObjectId,
) -> std::result::Result<ObjectId, MergeBaseForDiffError> {
    let bases = merge_bases_first_vs_rest(repo, other, &[head])
        .map_err(|e| MergeBaseForDiffError::Other(e.to_string()))?;
    match bases.len() {
        0 => Err(MergeBaseForDiffError::None),
        1 => Ok(bases[0]),
        _ => Err(MergeBaseForDiffError::Multiple),
    }
}

/// Merge base of two commits, matching `git diff --merge-base <a> <b>` / `diff-tree --merge-base`.
///
/// Returns an error when there is no merge base or more than one.
pub fn merge_base_for_diff_two_commits(
    repo: &Repository,
    a: ObjectId,
    b: ObjectId,
) -> std::result::Result<ObjectId, MergeBaseForDiffError> {
    let bases = merge_bases_first_vs_rest(repo, a, &[b])
        .map_err(|e| MergeBaseForDiffError::Other(e.to_string()))?;
    match bases.len() {
        0 => Err(MergeBaseForDiffError::None),
        1 => Ok(bases[0]),
        _ => Err(MergeBaseForDiffError::Multiple),
    }
}

/// Failure modes for [`merge_base_for_diff_index`] and [`merge_base_for_diff_two_commits`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MergeBaseForDiffError {
    /// No common ancestor between the commits.
    None,
    /// More than one minimal merge base (criss-cross history).
    Multiple,
    /// Resolution or object read error; message is suitable for stderr.
    Other(String),
}

/// Compute merge bases common to all supplied commits (`--octopus` mode).
///
/// # Parameters
///
/// - `repo` - repository used to walk commit parents.
/// - `commits` - commits to intersect.
///
/// # Errors
///
/// Returns parse and object read errors from commit traversal.
pub fn merge_bases_octopus(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let mut cache = CommitGraphCache::new(repo);
    let mut iter = commits.iter();
    let Some(&first) = iter.next() else {
        return Ok(Vec::new());
    };
    let mut common = cache.ancestor_closure(first)?;
    for &oid in iter {
        let set = cache.ancestor_closure(oid)?;
        common.retain(|item| set.contains(item));
    }
    let mut bases = reduce_to_best(common, &mut cache)?;
    sort_merge_bases_for_git_output(&mut cache, &mut bases)?;
    Ok(bases)
}

/// All merge bases common to every supplied commit (intersection of ancestor sets,
/// reduced to minimal bases). Matches `git merge-base` with two or more tips.
///
/// This is the same intersection-and-reduction as [`merge_bases_octopus`]; the name
/// documents the `git merge-base A B C ...` calling convention.
///
/// For two commits the bases come newest committer date first, and bases with equal
/// dates in the order Git reports them (the order its history walk reaches them).
pub fn merge_bases_all(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    if commits.len() == 1 {
        return Ok(vec![commits[0]]);
    }
    if commits.len() == 2 {
        let mut cache = CommitGraphCache::new(repo);
        // A tip that is an ancestor of the other is the only merge base: every other common
        // ancestor is one of its own ancestors.
        match direct_ancestor_relation(&mut cache, commits[0], commits[1])? {
            FirstParentRelation::Same | FirstParentRelation::FirstIsAncestor => {
                return Ok(vec![commits[0]]);
            }
            FirstParentRelation::SecondIsAncestor => return Ok(vec![commits[1]]),
            FirstParentRelation::Unrelated => {}
        }
        return all_merge_bases_vs_union(&mut cache, commits[0], &commits[1..2]);
    }
    merge_bases_octopus(repo, commits)
}

/// Every merge base of `one` and a hypothetical merge of all `others`, newest committer first.
///
/// The hypothetical merge's history is the union of the histories of `others`, so this is
/// also the merge-base set of `one` and an in-memory commit whose parents are `others`
/// (what a recursive merge needs after merging several merge bases into a virtual one).
/// Unlike [`merge_bases_first_vs_rest`] the result is not truncated to one base.
pub(crate) fn merge_bases_all_vs_union(
    repo: &Repository,
    one: ObjectId,
    others: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    all_merge_bases_vs_union(&mut CommitGraphCache::new(repo), one, others)
}

fn all_merge_bases_vs_union(
    cache: &mut CommitGraphCache<'_>,
    one: ObjectId,
    others: &[ObjectId],
) -> Result<Vec<ObjectId>> {
    // The walk yields bases newest first and, among equal dates, in discovery order (Git's
    // order); dropping redundant ones keeps that order, which a set would lose.
    let mut walked = walk_intersecting_histories(cache, one, others, true)?;
    if walked.len() > 1 {
        let candidates: OidHashSet = walked.iter().copied().collect();
        let best: OidHashSet = reduce_to_best(candidates, cache)?.into_iter().collect();
        walked.retain(|oid| best.contains(oid));
    }
    Ok(walked)
}

/// Check whether `ancestor` is reachable from `descendant`.
///
/// # Errors
///
/// Returns parse and object read errors from commit traversal.
pub fn is_ancestor(repo: &Repository, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut cache = CommitGraphCache::new(repo);
    cache.is_ancestor(ancestor, descendant)
}

/// [`is_ancestor`] that first looks for `descendant` on `ancestor`'s first-parent chain, the
/// shape of a ref moved back with `HEAD~n`.
///
/// Finding it proves `descendant` is a strict ancestor of `ancestor`, so the answer is `false`
/// (history has no cycles) without [`is_ancestor`]'s date-slop walk, which would otherwise read
/// a hundred commits below `ancestor`'s date. The chain walk stops at the first commit dated
/// before `descendant`, so a ref that moved anywhere else costs only the few commits newer
/// than `descendant`; [`is_ancestor`] then decides. Grafts and replace refs can make ancestry
/// cyclic, so with either present the chain is not consulted.
///
/// # Errors
///
/// Propagates commit read and parse errors.
pub(crate) fn is_ancestor_unless_rewound(
    repo: &Repository,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut cache = CommitGraphCache::new(repo);
    if !cache.has_parent_rewrites()?
        && first_parent_chain_reaches_newer(&mut cache, ancestor, descendant).unwrap_or(false)
    {
        return Ok(false);
    }
    cache.is_ancestor(ancestor, descendant)
}

/// Whether `target` lies on `from`'s first-parent chain, walking only commits dated at or
/// after `target`.
fn first_parent_chain_reaches_newer(
    cache: &mut CommitGraphCache<'_>,
    from: ObjectId,
    target: ObjectId,
) -> Result<bool> {
    let target_time = cache.commit_time(target)?;
    let mut cur = from;
    while cache.commit_time(cur)? >= target_time {
        let Some(&parent) = cache.parents_of(cur)?.first() else {
            return Ok(false);
        };
        if parent == target {
            return Ok(true);
        }
        cur = parent;
    }
    Ok(false)
}

/// Whether the acknowledged haves of a fetch negotiation cover every wanted
/// commit, so upload-pack may answer `ready` and send the pack.
///
/// Each have commit and its immediate parents count as held by the client.
/// A want is covered when walking its parents reaches a held commit. Commits
/// older (by committer date) than the oldest have are not walked through,
/// which keeps the check to the few commits between the wants and the haves
/// instead of a history scan. Parents and dates come from the commit-graph
/// when the repository has one.
///
/// # Parameters
///
/// - `repo` - repository being served.
/// - `wants` - wanted tips, already peeled through tags; only commits.
/// - `have_commits` - acknowledged haves that are commits (tags are not peeled).
///
/// # Returns
///
/// `false` when `have_commits` is empty or some want cannot reach a held
/// commit within the date cutoff; `true` otherwise.
///
/// # Errors
///
/// Returns object read and parse errors for commits on the walk.
pub(crate) fn wants_covered_by_haves(
    repo: &Repository,
    wants: &[ObjectId],
    have_commits: &[ObjectId],
) -> Result<bool> {
    if have_commits.is_empty() {
        return Ok(false);
    }
    let mut cache = CommitGraphCache::new(repo);
    // Commits that reach a held commit; grows with each covered want's path.
    let mut covered = oid_hash_set();
    let mut oldest_have = i64::MAX;
    for &have in have_commits {
        covered.insert(have);
        covered.extend(cache.parents_of(have)?);
        oldest_have = oldest_have.min(cache.commit_time(have)?);
    }
    // Commits whose walk finished without reaching a held commit.
    let mut explored = oid_hash_set();
    for &want in wants {
        if covered.contains(&want) {
            continue;
        }
        if !explored.insert(want) {
            return Ok(false);
        }
        let mut path = vec![(want, cache.parents_of(want)?, 0usize)];
        let mut reached = false;
        while let Some((_, parents, next)) = path.last_mut() {
            let Some(&parent) = parents.get(*next) else {
                path.pop();
                continue;
            };
            *next += 1;
            if covered.contains(&parent) {
                reached = true;
                break;
            }
            if cache.commit_time(parent)? < oldest_have || !explored.insert(parent) {
                continue;
            }
            let grandparents = cache.parents_of(parent)?;
            path.push((parent, grandparents, 0));
        }
        if !reached {
            return Ok(false);
        }
        covered.extend(path.into_iter().map(|(oid, _, _)| oid));
    }
    Ok(true)
}

/// Reachability test using only the given object database (no commit-graph,
/// grafts or shallow boundaries).
///
/// Works on any [`Odb`](crate::odb::Odb), such as a receive quarantine whose
/// alternates include the repository's own objects, so it can check commits
/// that are not yet visible through [`Repository::odb`].
///
/// History is walked from both commits at once, newest committer date first.
/// The walk ends once every line of history from `descendant` has either
/// reached `ancestor` or joined `ancestor`'s own history, so a rejected
/// fast-forward costs about the distance to the fork point. Committer dates
/// only order the walk; the answer stays exact when they are skewed.
///
/// # Parameters
///
/// - `odb` - object database holding both commits and the history between them.
/// - `ancestor` - candidate ancestor commit.
/// - `descendant` - commit whose history is searched.
///
/// # Returns
///
/// `true` when `ancestor` equals or is reachable from `descendant`; `false`
/// otherwise, including when either object is not a commit. Parents missing
/// from `odb` end their line of history.
///
/// # Errors
///
/// Returns read errors for `ancestor` or `descendant`, and parse errors for
/// commits on the walk.
pub fn is_ancestor_odb_walk(
    odb: &crate::odb::Odb,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    is_ancestor_odb_walk_with_graph(odb, None, ancestor, descendant)
}

/// [`is_ancestor_odb_walk`] that also uses `graph`, the commit-graph of the
/// repository whose objects `odb` contains (directly or as an alternate).
///
/// Commits found in `graph` take their parents from it without an object
/// read. When `ancestor` has a corrected commit date (generation v2), any
/// graph commit with a generation at or below it cannot reach `ancestor`, so
/// its history is skipped. Generations strictly increase from parent to
/// child, which makes this cutoff exact; commits outside the graph (such as
/// freshly pushed ones) are always expanded.
pub(crate) fn is_ancestor_odb_walk_with_graph(
    odb: &crate::odb::Odb,
    graph: Option<&crate::commit_graph_file::CommitGraphChain>,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let Some(ancestor_linkage) = read_commit_linkage(odb, graph, &ancestor)? else {
        return Ok(false);
    };
    let Some(descendant_linkage) = read_commit_linkage(odb, graph, &descendant)? else {
        return Ok(false);
    };
    let ancestor_generation = graph
        .map(|g| g.commit_generation_for_walk(&ancestor))
        .filter(|&generation| generation != UNRANKED_COMMIT);
    let cannot_reach_ancestor = |oid: &ObjectId| {
        let (Some(graph), Some(limit)) = (graph, ancestor_generation) else {
            return false;
        };
        let generation = graph.commit_generation_for_walk(oid);
        generation != UNRANKED_COMMIT && generation <= limit
    };

    // Paint everything reachable from `ancestor` with FROM_ANCESTOR and from
    // `descendant` with FROM_DESCENDANT. `ancestor` is reachable exactly when it
    // gets FROM_DESCENDANT. Only a commit painted FROM_DESCENDANT alone can
    // still pass that paint on to `ancestor`: a commit with both paints is an
    // ancestor of `ancestor`, so nothing it reaches is `ancestor` itself. The
    // walk therefore ends once no queued commit is painted FROM_DESCENDANT alone,
    // whatever order committer dates put the commits in.
    const FROM_ANCESTOR: u8 = 1;
    const FROM_DESCENDANT: u8 = 2;
    let mut paint: OidHashMap<u8> = oid_hash_map();
    let mut parents_of: OidHashMap<Vec<ObjectId>> = oid_hash_map();
    let mut times: OidHashMap<i64> = oid_hash_map();
    let mut descendant_only = oid_hash_set();
    // Expand by generation, then committer date. Commits outside the graph (such as the
    // ones being pushed) rank above every graph commit, so a descendant dated before the
    // ancestor's history reaches it without first draining that history. Without a graph
    // every commit ranks the same and this is the plain date order.
    let rank =
        |oid: &ObjectId| graph.map_or(UNRANKED_COMMIT, |g| g.commit_generation_for_walk(oid));
    let mut heap = BinaryHeap::new();
    for (oid, color, (time, parents)) in [
        (ancestor, FROM_ANCESTOR, ancestor_linkage),
        (descendant, FROM_DESCENDANT, descendant_linkage),
    ] {
        paint.insert(oid, color);
        parents_of.insert(oid, parents);
        times.insert(oid, time);
        heap.push((rank(&oid), time, oid));
    }
    descendant_only.insert(descendant);

    while !descendant_only.is_empty() {
        let Some((_, _, oid)) = heap.pop() else {
            break;
        };
        descendant_only.remove(&oid);
        let color = paint.get(&oid).copied().unwrap_or_default();
        let parents = parents_of.get(&oid).cloned().unwrap_or_default();
        for parent in parents {
            let old_color = paint.get(&parent).copied().unwrap_or_default();
            let new_color = old_color | color;
            if new_color == old_color {
                continue;
            }
            if parent == ancestor {
                return Ok(true);
            }
            if new_color == FROM_DESCENDANT && cannot_reach_ancestor(&parent) {
                continue;
            }
            let time = match times.get(&parent) {
                Some(&time) => time,
                None => {
                    let linkage = match read_commit_linkage(odb, graph, &parent) {
                        Err(Error::ObjectNotFound(_)) => None,
                        other => other?,
                    };
                    let Some((time, grandparents)) = linkage else {
                        continue;
                    };
                    times.insert(parent, time);
                    parents_of.insert(parent, grandparents);
                    time
                }
            };
            paint.insert(parent, new_color);
            if new_color == FROM_DESCENDANT {
                descendant_only.insert(parent);
            } else {
                descendant_only.remove(&parent);
            }
            heap.push((rank(&parent), time, parent));
        }
    }
    Ok(false)
}

/// Committer timestamp and parents of `oid`, or `None` when it is not a commit.
///
/// Reads them from `graph` when it lists `oid`, otherwise from `odb`.
fn read_commit_linkage(
    odb: &crate::odb::Odb,
    graph: Option<&crate::commit_graph_file::CommitGraphChain>,
    oid: &ObjectId,
) -> Result<Option<(i64, Vec<ObjectId>)>> {
    if let Some((parents, time)) = graph.and_then(|g| g.graph_commit(oid)) {
        return Ok(Some((time, parents)));
    }
    let object = odb.read(oid)?;
    if object.kind != ObjectKind::Commit {
        return Ok(None);
    }
    let commit = CommitRef::parse(&object.data)?;
    Ok(Some((
        commit.committer_timestamp(),
        commit.parents().to_vec(),
    )))
}

/// Returns the ref path under `logs/` used for fork-point reflog scanning for `merge-base --fork-point`
/// and `rebase --fork-point`, matching Git's resolution order.
///
/// # Parameters
///
/// - `spec` - upstream argument as given on the command line (`main`, `refs/heads/main`, `HEAD`, …).
pub fn resolve_fork_point_reflog_ref(repo: &Repository, spec: &str) -> String {
    if spec == "HEAD" || spec.starts_with("refs/") {
        return spec.to_string();
    }

    let logs_dir = repo.git_dir().join("logs");
    let candidates = [
        spec.to_string(),
        format!("refs/heads/{spec}"),
        format!("refs/remotes/{spec}"),
    ];

    for candidate in candidates {
        if logs_dir.join(&candidate).is_file() {
            return candidate;
        }
    }

    format!("refs/heads/{spec}")
}

/// Picks the fork-point candidate that is not strictly dominated by another candidate in the list.
fn select_best_fork_point(repo: &Repository, candidates: &[ObjectId]) -> Result<Option<ObjectId>> {
    if candidates.is_empty() {
        return Ok(None);
    }

    let mut best = oid_hash_set();
    for &candidate in candidates {
        let mut dominated = false;
        for &other in candidates {
            if candidate == other {
                continue;
            }
            if is_ancestor(repo, candidate, other)? {
                dominated = true;
                break;
            }
        }
        if !dominated {
            best.insert(candidate);
        }
    }

    Ok(candidates.iter().copied().find(|oid| best.contains(oid)))
}

/// Computes the fork-point commit between `upstream_tip` and `head`, using the upstream ref's reflog.
///
/// This matches `git merge-base --fork-point` / the merge base `git rebase --fork-point` uses for
/// selecting commits to replay.
///
/// # Parameters
///
/// - `upstream_spec` - upstream revision string (used to locate the reflog; e.g. `main`,
///   `refs/heads/main`, or `topic@{{upstream}}`).
/// - `upstream_tip` - resolved commit of the upstream branch tip.
/// - `head` - commit to rebase (usually `HEAD`).
///
/// # Errors
///
/// Propagates object read, reflog, and graph walk errors.
pub fn fork_point(
    repo: &Repository,
    upstream_spec: &str,
    upstream_tip: ObjectId,
    head: ObjectId,
) -> Result<ObjectId> {
    let reflog_ref = if upstream_suffix_info(upstream_spec).is_some() {
        resolve_upstream_symbolic_name(repo, upstream_spec)?
    } else {
        resolve_fork_point_reflog_ref(repo, upstream_spec)
    };

    let entries = read_reflog(repo.git_dir(), &reflog_ref)
        .map_err(|e| Error::Message(format!("failed to read reflog for '{reflog_ref}': {e}")))?;

    let mut candidates = Vec::new();
    let mut seen = oid_hash_set();

    for entry in entries.iter().rev() {
        let oid = if entry.message.starts_with("checkout:") {
            entry.old_oid
        } else {
            entry.new_oid
        };
        if !seen.insert(oid) {
            continue;
        }
        if is_ancestor(repo, oid, head)? {
            candidates.push(oid);
        }
    }

    if let Some(fp) = select_best_fork_point(repo, &candidates)? {
        return Ok(fp);
    }

    let mut bases = merge_bases_first_vs_rest(repo, upstream_tip, &[head])?;
    if bases.is_empty() {
        return Err(Error::Message(
            "no merge base found between upstream and HEAD".to_owned(),
        ));
    }
    bases.sort();
    Ok(bases[0])
}

/// Returns every commit reachable from `tip` by walking parent links (including `tip`).
///
/// # Errors
///
/// Returns [`Error::CorruptObject`] if an encountered object is not a commit.
pub fn ancestor_closure(repo: &Repository, tip: ObjectId) -> Result<HashSet<ObjectId>> {
    let mut cache = CommitGraphCache::new(repo);
    Ok(cache.ancestor_closure(tip)?.into_iter().collect())
}

/// Count symmetric-diff commits between two tips, matching `git rev-list --left-right A...B`.
///
/// Returns `(ahead, behind)` where `ahead` counts commits reachable from `local` but not from
/// `other`, and `behind` the converse. Shared history is excluded from both counts.
///
/// # Errors
///
/// Propagates errors from commit graph walks.
pub fn count_symmetric_ahead_behind(
    repo: &Repository,
    local: ObjectId,
    other: ObjectId,
) -> Result<(usize, usize)> {
    let (ahead, behind, _, _) = ahead_behind_of_target_commits(repo, local, other, 0, 0)?;
    Ok((ahead, behind))
}

/// Symmetric ahead/behind between `head` and `target`, plus bounded newest OIDs on each side.
///
/// Matches `git rev-list --left-right --count HEAD...TARGET` for the counts. Both sides are
/// counted in one two-sided walk that stops at the common frontier, so the cost scales with
/// the commits since the histories diverged rather than with total history. The returned
/// OID lists contain up to `retain_newest_ahead` / `retain_newest_behind` commits reachable
/// from `head` / `target` but not the other tip (newest first, `git rev-list` order).
///
/// # Errors
///
/// Propagates object read and parse errors from commit traversal.
pub fn ahead_behind_of_target_commits(
    repo: &Repository,
    head: ObjectId,
    target: ObjectId,
    retain_newest_ahead: usize,
    retain_newest_behind: usize,
) -> Result<(usize, usize, Vec<ObjectId>, Vec<ObjectId>)> {
    if head == target {
        return Ok((0, 0, Vec::new(), Vec::new()));
    }
    let mut cache = CommitGraphCache::new(repo);
    let paint = SidePaint::compute(&mut cache, &[head], &[target])?;
    let ahead = paint.count_only(PAINT_LEFT);
    let behind = paint.count_only(PAINT_RIGHT);
    let behind_paint = (retain_newest_behind > 0 && behind > 0).then(|| paint.swapped());
    let ahead_oids = if retain_newest_ahead > 0 && ahead > 0 {
        // Reuse the paint so the ordered walk only filters, never re-walks history.
        let mut state = ReachableWalkState::new(repo, &[head], &[target])?;
        state.paint = Some(paint);
        state.emit_until(
            repo,
            Some(retain_newest_ahead),
            ReachableWalkCommitterCutoffs::default(),
        )?
    } else {
        Vec::new()
    };
    let behind_oids = if let Some(behind_paint) = behind_paint {
        let mut state = ReachableWalkState::new(repo, &[target], &[head])?;
        state.paint = Some(behind_paint);
        state.emit_until(
            repo,
            Some(retain_newest_behind),
            ReachableWalkCommitterCutoffs::default(),
        )?
    } else {
        Vec::new()
    };
    Ok((ahead, behind, ahead_oids, behind_oids))
}

/// Commits reachable from `tips` that are not ancestors of any `hide` tip, in committer-date order
/// (newest first). Matches `git rev-list tips ^hide…` default ordering for the common case without
/// materializing the full ancestor closure of the hide tips.
///
/// When `limit` is [`Some`], traversal stops once that many commits have been collected (after
/// applying the hide filter). Use this for bounded ahead-of-target lists and status shortlogs.
///
/// Shallow boundaries (`.git/shallow`) stop parent traversal without following
/// absent parent objects. Other missing commits propagate [`Error::ObjectNotFound`].
///
/// # Errors
///
/// Propagates object read and parse errors from commit traversal.
/// Committer-date filters applied during bounded reachable walks (Git `--since` / `--until`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReachableWalkCommitterCutoffs {
    /// Exclude commits with committer date strictly before this Unix timestamp (`--since`).
    pub since: Option<i64>,
    /// Exclude commits with committer date strictly after this Unix timestamp (`--until`).
    pub until: Option<i64>,
}

/// How [`walk_commits_reachable_excluding_ancestors_of`] bounds its output.
#[derive(Debug, Clone, Copy)]
pub enum ReachableWalkLimit {
    /// Emit at most `n` commits and stop (rev-list `--max-count`).
    StopAfter(usize),
    /// Walk the full emitted set; return the total count and only the newest `n` OIDs.
    CountAllRetainNewest(usize),
    /// Walk and collect every emitted commit.
    Unlimited,
}

/// Max-heap key for Git default rev-list date ordering (newest committer time first).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalkDateKey {
    oid: ObjectId,
    time: i64,
    /// Insertion-order tiebreaker for equal dates (earlier insertion pops first).
    seq: u64,
}

impl Ord for WalkDateKey {
    fn cmp(&self, other: &Self) -> Ordering {
        match self.time.cmp(&other.time) {
            Ordering::Equal => match other.seq.cmp(&self.seq) {
                Ordering::Equal => self.oid.cmp(&other.oid),
                ord => ord,
            },
            ord => ord,
        }
    }
}

impl PartialOrd for WalkDateKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Resumable date-order walk state for [`walk_commits_reachable_excluding_ancestors_of`].
///
/// Ordering matches Git's default `rev-list`: pop the greatest committer timestamp (ties go
/// to the earlier queued commit) and queue each parent when it is first discovered. Nothing
/// waits for a commit's other children, so under clock skew a parent can print before one of
/// its children, exactly as in Git.
#[derive(Clone, Debug)]
pub struct ReachableWalkState {
    heap: BinaryHeap<WalkDateKey>,
    done: OidHashSet,
    queued: OidHashSet,
    /// Tips seeded on the heap up front (may emit before all children are shown).
    seeded: OidHashSet,
    seq: u64,
    hide: Vec<ObjectId>,
    /// Which pending commits reach a hide tip; computed on first use so restored
    /// snapshots rebuild it from their heap.
    paint: Option<SidePaint>,
    /// Read parents and dates from the on-disk commit-graph when one is present.
    use_commit_graph: bool,
    /// When the hide paint may stop (see [`HidePaintStop`]).
    hide_paint_stop: HidePaintStop,
    /// Dates the hide paint requires of every commit it pops as reachable from the tips alone.
    left_window: Option<LeftDateWindow>,
}

/// Committer dates (Unix seconds) a [`SidePaint::compute_hiding`] paint requires of every
/// commit it pops as reachable from the tips alone: later than `after`, no later than `until`.
/// The paint gives up at the first commit outside them ([`SidePaint::left_outside_window`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LeftDateWindow {
    after: i64,
    until: i64,
}

impl LeftDateWindow {
    fn contains(self, committed_at: i64) -> bool {
        self.after < committed_at && committed_at <= self.until
    }
}

/// When [`SidePaint::compute_hiding`] may stop painting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HidePaintStop {
    /// Paint until every queued commit is reachable from both sides. Exact under clock skew,
    /// but a hide tip that forked near the root keeps the walk going down to that fork.
    #[default]
    Converged,
    /// Stop once no queued commit is reachable from the tips alone, except that commits
    /// without a commit-graph generation keep the [`Self::Converged`] rule. Generations grow
    /// strictly from parent to child and the graph holds every ancestor of its commits, so a
    /// queued commit with a generation can never reach a commit already popped: the stop is
    /// exact. Committer dates promise nothing under clock skew, so without a commit-graph this
    /// is the converging walk.
    TipsResolved,
    /// [`Self::TipsResolved`] for commits without a generation too: a queued commit keeps the
    /// walk going only while the tips alone reach it, and once none does the walk ends by
    /// committer date (see [`PaintWalk::unranked_hide_may_reach_left`]). Like Git's slop-based
    /// `rev-list`, the result can be wrong under clock skew larger than
    /// [`UNRANKED_SKEW_ALLOWANCE`]. Only the orphaned-commit report, which is advisory, uses it,
    /// and only in repositories without a commit-graph.
    TipsResolvedDateSlop,
}

/// Serializable snapshot of [`ReachableWalkState`] for resuming a bounded walk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReachableWalkSnapshot {
    /// Pending heap entries (order does not matter).
    pub heap: Vec<(i64, u64, ObjectId)>,
    /// Commits already popped from the heap (emitted or skipped as duplicate/hidden).
    pub done: Vec<ObjectId>,
    /// Every OID ever queued on the heap.
    pub queued: Vec<ObjectId>,
    /// Tip OIDs seeded on the heap at walk start.
    pub seeded: Vec<ObjectId>,
    /// `seq` field.
    pub seq: u64,
    /// `hide` field.
    pub hide: Vec<ObjectId>,
}

impl ReachableWalkState {
    /// Initialize a walk from `tips`, excluding ancestors of `hide`.
    ///
    /// # Errors
    ///
    /// Propagates commit read errors while seeding the heap.
    pub fn new(repo: &Repository, tips: &[ObjectId], hide: &[ObjectId]) -> Result<Self> {
        Self::with_commit_graph(repo, tips, hide, true)
    }

    /// Like [`Self::new`]; `use_commit_graph: false` parses every commit object instead of
    /// reading the on-disk commit-graph ([`crate::rev_list::RevListOptions::use_commit_graph`]).
    pub(crate) fn with_commit_graph(
        repo: &Repository,
        tips: &[ObjectId],
        hide: &[ObjectId],
        use_commit_graph: bool,
    ) -> Result<Self> {
        let mut cache = CommitGraphCache::with_commit_graph(repo, use_commit_graph);
        Self::seed(&mut cache, tips, hide, use_commit_graph)
    }

    fn seed(
        cache: &mut CommitGraphCache<'_>,
        tips: &[ObjectId],
        hide: &[ObjectId],
        use_commit_graph: bool,
    ) -> Result<Self> {
        let mut state = Self {
            heap: BinaryHeap::new(),
            done: oid_hash_set(),
            queued: oid_hash_set(),
            seeded: oid_hash_set(),
            seq: 0,
            hide: hide.to_vec(),
            paint: None,
            use_commit_graph,
            hide_paint_stop: HidePaintStop::default(),
            left_window: None,
        };
        for &tip in tips {
            if state.hide.contains(&tip) {
                continue;
            }
            if state.queued.insert(tip) {
                state.push_on_heap(cache, tip, true)?;
            }
        }
        Ok(state)
    }

    /// Start (or resume from `resume`) a walk and emit up to `limit` commits, also returning
    /// the raw object bytes of every emitted commit whose object the walk had to read.
    ///
    /// Commits whose parents and dates came from the commit-graph (or an earlier walk's
    /// linkage cache, such as commits read while painting the hidden side) are absent from
    /// the map; callers read those themselves.
    pub(crate) fn page_retaining_commits(
        repo: &Repository,
        resume: Option<ReachableWalkSnapshot>,
        tips: &[ObjectId],
        hide: &[ObjectId],
        limit: usize,
        cutoffs: ReachableWalkCommitterCutoffs,
    ) -> Result<(Self, Vec<ObjectId>, WalkedCommits)> {
        // Under the pack-read context the walk reads every commit through one single-pack
        // reader instead of installing the context and opening a reader per object.
        repo.odb().with_pack_read_context(|| {
            Self::page_retaining_commits_in_context(repo, resume, tips, hide, limit, cutoffs)
        })
    }

    fn page_retaining_commits_in_context(
        repo: &Repository,
        resume: Option<ReachableWalkSnapshot>,
        tips: &[ObjectId],
        hide: &[ObjectId],
        limit: usize,
        cutoffs: ReachableWalkCommitterCutoffs,
    ) -> Result<(Self, Vec<ObjectId>, WalkedCommits)> {
        // Sized for the page up front so the id table never exists twice while it grows.
        const MAX_PRESIZE: usize = 1 << 16;
        let mut cache = CommitGraphCache::new(repo);
        cache.retained = Some(WalkedCommits::with_capacity(
            limit.saturating_add(16).min(MAX_PRESIZE),
        ));
        let mut state = match resume {
            Some(snapshot) => Self::from_snapshot(snapshot),
            None => Self::seed(&mut cache, tips, hide, true)?,
        };
        let commits = state.emit_with(&mut cache, Some(limit), cutoffs)?;
        let mut retained = cache.retained.take().unwrap_or_default();
        let emitted_present = commits.iter().filter(|oid| retained.contains(oid)).count();
        if emitted_present < retained.len() {
            let emitted: OidHashSet = commits.iter().copied().collect();
            retained.retain(|oid| emitted.contains(oid));
        }
        Ok((state, commits, retained))
    }

    /// Restore walk state from a [`ReachableWalkSnapshot`].
    #[must_use]
    pub fn from_snapshot(snapshot: ReachableWalkSnapshot) -> Self {
        let ReachableWalkSnapshot {
            heap,
            done,
            queued,
            seeded,
            seq,
            hide,
        } = snapshot;
        Self {
            heap: heap
                .into_iter()
                .map(|(time, seq, oid)| WalkDateKey { oid, time, seq })
                .collect(),
            done: done.into_iter().collect(),
            queued: queued.into_iter().collect(),
            seeded: seeded.into_iter().collect(),
            seq,
            paint: None,
            hide,
            use_commit_graph: true,
            hide_paint_stop: HidePaintStop::default(),
            left_window: None,
        }
    }

    /// Capture the current walk state for later resumption.
    #[must_use]
    pub fn snapshot(&self) -> ReachableWalkSnapshot {
        ReachableWalkSnapshot {
            heap: self
                .heap
                .iter()
                .map(|key| (key.time, key.seq, key.oid))
                .collect(),
            done: self.done.iter().copied().collect(),
            queued: self.queued.iter().copied().collect(),
            seeded: self.seeded.iter().copied().collect(),
            seq: self.seq,
            hide: self.hide.clone(),
        }
    }

    /// Whether the heap still holds commits that might be emitted.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.heap.is_empty()
    }

    /// Whether the walk queue may still produce commits that pass `cutoffs`.
    ///
    /// When `--since` excludes every queued commit (common after hitting `-n` while
    /// older commits remain on the heap), this returns `false` even though
    /// [`Self::has_pending`] is still true.
    #[must_use]
    pub fn has_emittable_pending(&self, cutoffs: ReachableWalkCommitterCutoffs) -> bool {
        if self.heap.is_empty() {
            return false;
        }
        if cutoffs
            .since
            .is_some_and(|since| self.heap.iter().all(|key| key.time < since))
        {
            return false;
        }
        true
    }

    /// Paint pending commits against the hide tips. Must run before the heap is
    /// popped: every commit still to be emitted is an ancestor of a heap entry.
    fn ensure_paint(&mut self, cache: &mut CommitGraphCache<'_>) -> Result<()> {
        if self.paint.is_some() || self.hide.is_empty() {
            return Ok(());
        }
        let pending: Vec<ObjectId> = self.heap.iter().map(|key| key.oid).collect();
        self.paint = Some(SidePaint::compute_hiding(
            cache,
            &pending,
            &self.hide,
            self.hide_paint_stop,
            self.left_window,
        )?);
        Ok(())
    }

    fn is_hidden(&mut self, cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> Result<bool> {
        if self.hide.is_empty() {
            return Ok(false);
        }
        if self.hide.contains(&oid) {
            return Ok(true);
        }
        self.ensure_paint(cache)?;
        let Some(paint) = &self.paint else {
            return Ok(false);
        };
        if let Some(reached) = paint.reached_by(oid, PAINT_RIGHT) {
            return Ok(reached);
        }
        // Tag tips were painted under their peeled commit.
        if self.seeded.contains(&oid) {
            let peeled = peel_to_commit_for_merge_base(cache.repo, oid).unwrap_or(oid);
            if let Some(reached) = paint.reached_by(peeled, PAINT_RIGHT) {
                return Ok(reached);
            }
        }
        // Unpainted commits sit below the frontier where every path is reachable
        // from a hide tip.
        Ok(true)
    }

    fn push_on_heap(
        &mut self,
        cache: &mut CommitGraphCache<'_>,
        oid: ObjectId,
        is_tip: bool,
    ) -> Result<()> {
        if is_tip {
            self.seeded.insert(oid);
        }
        let time = cache.commit_time_for_heap(oid)?;
        let seq = self.seq;
        self.seq += 1;
        self.heap.push(WalkDateKey { oid, time, seq });
        Ok(())
    }

    fn after_processed(&mut self, cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> Result<()> {
        for parent in cache.parents_of(oid)? {
            if self.queued.insert(parent) {
                // Ancestors of hidden commits are hidden too; keeping them off the
                // heap stops the walk at the hide frontier instead of the root.
                if self.is_hidden(cache, parent)? {
                    self.done.insert(parent);
                    continue;
                }
                self.push_on_heap(cache, parent, false)?;
            }
        }
        Ok(())
    }

    fn pop_next_emit(
        &mut self,
        cache: &mut CommitGraphCache<'_>,
        cutoffs: ReachableWalkCommitterCutoffs,
    ) -> Result<Option<ObjectId>> {
        self.ensure_paint(cache)?;
        loop {
            let Some(key) = self.heap.pop() else {
                return Ok(None);
            };
            let oid = key.oid;
            if !self.done.insert(oid) {
                continue;
            }
            if cutoffs.since.is_some_and(|since| key.time < since) {
                // Match Git: when every other queued commit is also older than `--since`, stop
                // before enqueueing this commit's parents. Otherwise keep walking for clock skew.
                let stop = self.heap.is_empty()
                    || self
                        .heap
                        .iter()
                        .all(|pending| pending.time < cutoffs.since.unwrap_or(i64::MIN));
                if stop {
                    self.heap.clear();
                    return Ok(None);
                }
                self.after_processed(cache, oid)?;
                continue;
            }
            let hidden = self.is_hidden(cache, oid)?;
            if hidden {
                self.after_processed(cache, oid)?;
                continue;
            }
            if cutoffs.until.is_some_and(|until| key.time > until) {
                self.after_processed(cache, oid)?;
                continue;
            }
            self.after_processed(cache, oid)?;
            return Ok(Some(oid));
        }
    }

    /// Emit up to `limit` commits (newest first), stopping early when `limit` is reached.
    ///
    /// # Errors
    ///
    /// Propagates object read and ancestry errors during traversal.
    pub fn emit_until(
        &mut self,
        repo: &Repository,
        limit: Option<usize>,
        cutoffs: ReachableWalkCommitterCutoffs,
    ) -> Result<Vec<ObjectId>> {
        let mut cache = CommitGraphCache::with_commit_graph(repo, self.use_commit_graph);
        self.emit_with(&mut cache, limit, cutoffs)
    }

    fn emit_with(
        &mut self,
        cache: &mut CommitGraphCache<'_>,
        limit: Option<usize>,
        cutoffs: ReachableWalkCommitterCutoffs,
    ) -> Result<Vec<ObjectId>> {
        let mut out = Vec::new();
        while let Some(oid) = self.pop_next_emit(cache, cutoffs)? {
            out.push(oid);
            if limit.is_some_and(|n| out.len() >= n) {
                break;
            }
        }
        Ok(out)
    }
}

/// Walk commits reachable from `tips` that are not ancestors of any `hide` tip.
///
/// Returns `(emitted_count, collected_oids)` where `collected_oids` are newest-first.
/// With [`ReachableWalkLimit::CountAllRetainNewest`], `emitted_count` is the full
/// ahead count while `collected_oids` holds at most the requested number of newest tips.
pub fn walk_commits_reachable_excluding_ancestors_of(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    limit: ReachableWalkLimit,
) -> Result<(usize, Vec<ObjectId>)> {
    walk_commits_reachable_excluding_ancestors_of_cutoffs(
        repo,
        tips,
        hide,
        limit,
        ReachableWalkCommitterCutoffs::default(),
    )
}

/// Like [`walk_commits_reachable_excluding_ancestors_of`] with committer-date cutoffs on bounded walks.
pub fn walk_commits_reachable_excluding_ancestors_of_cutoffs(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    limit: ReachableWalkLimit,
    cutoffs: ReachableWalkCommitterCutoffs,
) -> Result<(usize, Vec<ObjectId>)> {
    if tips.is_empty() {
        return Ok((0, Vec::new()));
    }

    let mut state = ReachableWalkState::new(repo, tips, hide)?;
    let (stop_early_at, retain_newest) = match limit {
        ReachableWalkLimit::StopAfter(n) => (Some(n), None),
        ReachableWalkLimit::CountAllRetainNewest(n) => (None, Some(n)),
        ReachableWalkLimit::Unlimited => (None, None),
    };

    if let Some(k) = retain_newest {
        let mut cache = CommitGraphCache::new(repo);
        let mut out = Vec::new();
        let mut emitted = 0usize;
        while let Some(oid) = state.pop_next_emit(&mut cache, cutoffs)? {
            emitted += 1;
            if out.len() < k {
                out.push(oid);
            }
        }
        return Ok((emitted, out));
    }

    let out = state.emit_until(repo, stop_early_at, cutoffs)?;
    Ok((out.len(), out))
}

/// Count the commits reachable from `tips` that no `hide` tip reaches, keeping the newest
/// `retain_newest` of them: the commits a ref move leaves behind.
///
/// Same result as [`walk_commits_reachable_excluding_ancestors_of`] with
/// [`ReachableWalkLimit::CountAllRetainNewest`] when every commit involved has a commit-graph
/// generation: the hide paint stops as soon as `tips` are resolved, so a hidden ref that forked
/// from `tips` near the root costs nothing, which matters because callers hide every ref in the
/// repository. Without a commit-graph the paint ends by committer date instead
/// ([`HidePaintStop::TipsResolvedDateSlop`]), so the count behaves like Git's slop-based
/// `rev-list` and can be wrong under heavy clock skew. Grafts and shallow boundaries rewrite
/// parents behind the graph's back, so they keep the converging paint.
///
/// # Errors
///
/// Propagates commit read and parse errors from the walk.
pub(crate) fn count_commits_left_behind(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    retain_newest: usize,
) -> Result<(usize, Vec<ObjectId>)> {
    let mut state = ReachableWalkState::new(repo, tips, hide)?;
    let mut cache = CommitGraphCache::new(repo);
    if cache.graft_parents.is_empty() && cache.shallow_boundaries.is_empty() {
        state.hide_paint_stop = if cache.graph.is_some() {
            HidePaintStop::TipsResolved
        } else {
            HidePaintStop::TipsResolvedDateSlop
        };
    }
    drain_left_behind(&mut state, &mut cache, retain_newest)
}

/// [`count_commits_left_behind`] with `hide` missing the refs a store recorded at
/// `recorded_at` (Unix seconds), such as the records of `packed-refs`, or `None` when the
/// answer could differ from the one the full hide set gives.
///
/// A recorded ref names a commit that existed at `recorded_at`, so everything it reaches was
/// made before then and, within the clock-skew allowance the date-slop paint already relies on
/// ([`UNRANKED_SKEW_ALLOWANCE`]), is dated at most that much after it. When every commit left
/// behind is dated later than that, the omitted refs reach none of them. A commit dated more
/// than the allowance past `now` says nothing about when it was made, so it ends the shortcut
/// too. The paint stops at the first commit outside those dates, so a `None` costs no more
/// than the history dated after `recorded_at`.
///
/// Also `None` when the walk does not end by date: with a commit-graph (whose exact paint
/// needs no reads for the omitted tips), grafts, replace refs or shallow boundaries. Like
/// [`count_commits_left_behind`] without a commit-graph, the count can be wrong under heavy
/// clock skew.
///
/// # Errors
///
/// Propagates commit read and parse errors from the walk.
pub(crate) fn count_commits_left_behind_after(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    retain_newest: usize,
    recorded_at: i64,
    now: i64,
) -> Result<Option<(usize, Vec<ObjectId>)>> {
    let mut cache = CommitGraphCache::new(repo);
    if hide.is_empty()
        || cache.graph.is_some()
        || !cache.shallow_boundaries.is_empty()
        || cache.has_parent_rewrites()?
    {
        return Ok(None);
    }
    let allowance = UNRANKED_SKEW_ALLOWANCE.whole_seconds();
    let mut state = ReachableWalkState::new(repo, tips, hide)?;
    state.hide_paint_stop = HidePaintStop::TipsResolvedDateSlop;
    state.left_window = Some(LeftDateWindow {
        after: recorded_at.saturating_add(allowance),
        until: now.saturating_add(allowance),
    });
    state.ensure_paint(&mut cache)?;
    if state
        .paint
        .as_ref()
        .is_none_or(|paint| paint.left_outside_window)
    {
        return Ok(None);
    }
    drain_left_behind(&mut state, &mut cache, retain_newest).map(Some)
}

/// Emit every commit `state` leaves after its hide paint: the full count and the newest
/// `retain_newest` of them.
fn drain_left_behind(
    state: &mut ReachableWalkState,
    cache: &mut CommitGraphCache<'_>,
    retain_newest: usize,
) -> Result<(usize, Vec<ObjectId>)> {
    let mut newest = Vec::new();
    let mut total = 0usize;
    while let Some(oid) = state.pop_next_emit(cache, ReachableWalkCommitterCutoffs::default())? {
        total += 1;
        if newest.len() < retain_newest {
            newest.push(oid);
        }
    }
    Ok((total, newest))
}

/// Count commits reachable from `head` but not from `target`, and return up to `retain_newest`
/// of those commit OIDs (newest first) without materializing the full ahead list.
///
/// # Errors
///
/// Propagates object read and parse errors from commit traversal.
pub fn ahead_of_target_commits(
    repo: &Repository,
    head: ObjectId,
    target: ObjectId,
    retain_newest: usize,
) -> Result<(usize, Vec<ObjectId>)> {
    if head == target {
        return Ok((0, Vec::new()));
    }
    walk_commits_reachable_excluding_ancestors_of(
        repo,
        std::slice::from_ref(&head),
        &[target],
        ReachableWalkLimit::CountAllRetainNewest(retain_newest),
    )
}

/// Filters applied when counting reachable commits (parent count and committer dates).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReachableCountFilter {
    /// Minimum parent count for a commit to be included.
    pub min_parents: Option<usize>,
    /// Maximum parent count for a commit to be included.
    pub max_parents: Option<usize>,
    /// Follow only the first parent link when walking.
    pub first_parent_only: bool,
    /// Exclude commits with committer date strictly before this Unix timestamp.
    pub since: Option<i64>,
    /// Exclude commits with committer date strictly after this Unix timestamp.
    pub until: Option<i64>,
}

fn try_count_diff_via_stored_commit_bitmaps(
    repo: &Repository,
    tip: ObjectId,
    hide: ObjectId,
) -> Result<Option<usize>> {
    let Some(index) = repo
        .odb()
        .with_pack_read_context(|| BitmapIndex::open(repo))
        .map_err(|e| Error::CorruptObject(format!("pack bitmap: {e}")))?
    else {
        return Ok(None);
    };
    let Some(tip_bm) = index.commit_bitmap(&tip) else {
        return Ok(None);
    };
    let Some(hide_bm) = index.commit_bitmap(&hide) else {
        return Ok(None);
    };
    let mut reachable = Bitmap::new();
    tip_bm.or_into(&mut reachable);
    let mut hidden = Bitmap::new();
    hide_bm.or_into(&mut hidden);
    reachable.and_not_assign(&hidden);
    let mut commit_type = Bitmap::new();
    index
        .type_bitmap(ObjectKind::Commit)
        .expand_into_bitmap(&mut commit_type)
        .map_err(|e| Error::CorruptObject(format!("pack bitmap: {e}")))?;
    commit_type.and_assign(&reachable);
    Ok(Some(commit_type.count_ones()))
}

/// Count commits reachable from `tips` that are not ancestors of any `hide` tip.
///
/// One two-sided paint marks what `hide` reaches and stops at the common frontier, so the
/// cost follows the size of the range rather than the depth of history. Parent and date
/// filters only decide which painted commits are counted: like Git `rev-list`, the walk still
/// goes through commits they reject. Cheap shortcuts (a first-parent chain down to `hide`,
/// stored pack bitmaps, a whole-history commit-graph count) run first when commit-graph
/// generations cannot bound the paint.
///
/// # Errors
///
/// Propagates object read and ancestry errors during traversal.
pub fn count_commits_reachable_excluding_ancestors_of(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    filter: ReachableCountFilter,
) -> Result<usize> {
    if tips.is_empty() {
        return Ok(0);
    }
    let mut cache = CommitGraphCache::new(repo);

    let unfiltered = filter.min_parents.is_none()
        && filter.max_parents.is_none()
        && filter.since.is_none()
        && filter.until.is_none();
    // Generations resolve the included side without visiting older hidden ancestry.
    let generations_bound_paint = cache.graph.is_some() && !cache.has_parent_rewrites()?;
    if !generations_bound_paint || hide.is_empty() {
        if let Some(n) = try_count_without_paint(&mut cache, tips, hide, filter, unfiltered)? {
            return Ok(n);
        }
    }

    let paint = match (generations_bound_paint, filter.first_parent_only) {
        (true, true) => SidePaint::compute_hiding_first_parent(&mut cache, tips, hide)?,
        (true, false) => {
            SidePaint::compute_hiding(&mut cache, tips, hide, HidePaintStop::TipsResolved, None)?
        }
        (false, _) => {
            SidePaint::compute_hiding(&mut cache, tips, hide, HidePaintStop::Converged, None)?
        }
    };
    if unfiltered && !filter.first_parent_only {
        return Ok(paint.count_only(PAINT_LEFT));
    }

    // Every commit the tips reach without `hide` is painted left-only, so the filtered walk
    // tests membership in the paint instead of asking about ancestry per commit. Taking each
    // entry out of the map doubles as the visited set; unpainted commits are common history.
    let mut sides = paint.sides;
    let mut stack = Vec::with_capacity(tips.len());
    for &tip in tips {
        if let Some(tip) = try_peel_to_commit_for_merge_base(repo, tip)? {
            stack.push(tip);
        }
    }
    let mut count = 0usize;
    while let Some(oid) = stack.pop() {
        if sides.remove(&oid) != Some(PAINT_LEFT) {
            continue;
        }
        let mut parents = cache.parents_of(oid)?;
        let parent_count = parents.len();
        let time = cache.commit_time(oid)?;
        let in_range = filter.since.is_none_or(|since| time >= since)
            && filter.until.is_none_or(|until| time <= until);
        let parent_ok = parent_count >= filter.min_parents.unwrap_or(0)
            && parent_count <= filter.max_parents.unwrap_or(usize::MAX);
        if in_range && parent_ok {
            count += 1;
        }
        if filter.first_parent_only {
            parents.truncate(1);
        }
        stack.extend(parents);
    }
    Ok(count)
}

/// The first commit on each first-parent chain from `tips` that a `hide` tip reaches through
/// any parent: the cut points of a `--first-parent` range walk.
///
/// Git follows only first parents from the commits it lists, while excluded tips still hide
/// everything they reach. Every ancestor of a hidden commit is hidden too, so a first-parent
/// walk from `tips` that stops at the returned commits lists exactly the range. The paint ends
/// once the tips' first-parent chains meet hidden history when commit-graph generations bound
/// it; otherwise it converges like the range count does.
///
/// # Parameters
///
/// - `tips` - commits to list from, following first parents only.
/// - `hide` - excluded tips; their ancestry through every parent is hidden.
/// - `use_commit_graph` - read parents and generations from the on-disk commit-graph.
///
/// # Errors
///
/// Propagates commit read and parse errors for commits not reached from `hide`.
pub(crate) fn first_parent_hidden_frontier(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    use_commit_graph: bool,
) -> Result<HashSet<ObjectId>> {
    let mut cache = CommitGraphCache::with_commit_graph(repo, use_commit_graph);
    // Grafts and replacements rewrite parents behind the graph's back, so its generations
    // cannot order the paint.
    if cache.graph.is_some() && cache.has_parent_rewrites()? {
        cache = CommitGraphCache::with_commit_graph(repo, false);
    }
    let paint = SidePaint::compute_hiding_first_parent(&mut cache, tips, hide)?;

    let mut frontier = HashSet::new();
    let mut seen = oid_hash_set();
    let mut stack = tips.to_vec();
    while let Some(oid) = stack.pop() {
        if !seen.insert(oid) {
            continue;
        }
        // Unpainted chain commits lie below a commit both sides reach, so they are hidden.
        if paint.sides.get(&oid) != Some(&PAINT_LEFT) {
            frontier.insert(oid);
            continue;
        }
        if let Some(&parent) = cache.parents_of(oid)?.first() {
            stack.push(parent);
        }
    }
    Ok(frontier)
}

/// Shortcuts for [`count_commits_reachable_excluding_ancestors_of`] that avoid painting, or
/// `None` when the paint has to run.
fn try_count_without_paint(
    cache: &mut CommitGraphCache<'_>,
    tips: &[ObjectId],
    hide: &[ObjectId],
    filter: ReachableCountFilter,
    unfiltered: bool,
) -> Result<Option<usize>> {
    let single = (tips.len() == 1 && hide.len() == 1).then(|| (tips[0], hide[0]));
    let relation = single.map(|(tip, hide)| first_parent_chain_relation(cache, tip, hide));
    if matches!(
        relation,
        Some(FirstParentRelation::Same | FirstParentRelation::FirstIsAncestor)
    ) {
        return Ok(Some(0));
    }

    // The graph count follows first parents from `hide` too, so it only serves first-parent
    // walks that hide nothing.
    if unfiltered && (hide.is_empty() || !filter.first_parent_only) {
        if let Some(graph) = &cache.graph {
            let shallow: HashSet<ObjectId> = cache.shallow_boundaries.iter().copied().collect();
            if let Some(n) =
                graph.count_reachable_commits(tips, hide, filter.first_parent_only, &shallow)
            {
                return Ok(Some(n));
            }
        }
    }

    let Some((tip, hide)) = single else {
        return Ok(None);
    };
    // Walking a merge-free chain down to `hide` costs one commit per counted commit, so it
    // runs before the bitmap probe, which pays for opening and checking the bitmap file.
    if relation == Some(FirstParentRelation::SecondIsAncestor) {
        if let Some(n) = try_count_on_first_parent_chain_excluding_hide(cache, tip, hide, filter)? {
            return Ok(Some(n));
        }
    }
    if unfiltered && !filter.first_parent_only {
        return try_count_diff_via_stored_commit_bitmaps(cache.repo, tip, hide);
    }
    Ok(None)
}

/// Newest-first commits from `tip` applying committer-date cutoffs, stopping after `limit`
/// matches, but only when every visited commit has at most one parent (linear history).
/// Returns `None` when a merge is encountered so callers can fall back to a full-graph
/// bounded walk.
pub fn try_linear_chain_commits_with_cutoffs(
    repo: &Repository,
    tip: ObjectId,
    limit: usize,
    cutoffs: ReachableWalkCommitterCutoffs,
) -> Result<Option<Vec<ObjectId>>> {
    // Parent-chain order is not committer-date order; `--since` needs the heap walk's frontier
    // (see [`ReachableWalkState::pop_next_emit`]) so clock-skewed histories stay correct.
    if cutoffs.since.is_some() {
        return Ok(None);
    }
    let mut cache = CommitGraphCache::new(repo);
    let mut out = Vec::with_capacity(limit.min(64));
    let mut stack = vec![tip];
    while let Some(cur) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let parents = cache.parents_of(cur)?;
        if parents.len() > 1 {
            return Ok(None);
        }
        let time = cache.commit_time(cur)?;
        let below_since = cutoffs.since.is_some_and(|since| time < since);
        let above_until = cutoffs.until.is_some_and(|until| time > until);
        if !below_since && !above_until {
            out.push(cur);
        }
        if below_since {
            break;
        }
        if let Some(&parent) = parents.first() {
            stack.push(parent);
        }
    }
    Ok(Some(out))
}

/// Count commits on `tip`'s first-parent chain above `hide`.
///
/// The caller must already know that `hide` lies on `tip`'s first-parent chain
/// ([`FirstParentRelation::SecondIsAncestor`]). Returns `None` when a merge is met and the
/// full graph must be walked instead.
fn try_count_on_first_parent_chain_excluding_hide(
    cache: &mut CommitGraphCache<'_>,
    tip: ObjectId,
    hide: ObjectId,
    filter: ReachableCountFilter,
) -> Result<Option<usize>> {
    let mut count = 0usize;
    let mut cur = tip;
    while cur != hide {
        let parents = cache.parents_of(cur)?;
        if !filter.first_parent_only && parents.len() > 1 {
            return Ok(None);
        }
        let parent_count = parents.len();
        let time = cache.commit_time(cur)?;
        let in_range = filter.since.is_none_or(|since| time >= since)
            && filter.until.is_none_or(|until| time <= until);
        let parent_ok = parent_count >= filter.min_parents.unwrap_or(0)
            && parent_count <= filter.max_parents.unwrap_or(usize::MAX);
        if in_range && parent_ok {
            count += 1;
        }
        cur = first_parent_oid(cache, cur)?;
    }
    Ok(Some(count))
}

/// `commits_reachable_excluding_ancestors_of` function.
pub fn commits_reachable_excluding_ancestors_of(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    limit: Option<usize>,
) -> Result<Vec<ObjectId>> {
    commits_reachable_excluding_ancestors_of_cutoffs(
        repo,
        tips,
        hide,
        limit,
        ReachableWalkCommitterCutoffs::default(),
    )
}

/// Like [`commits_reachable_excluding_ancestors_of`] with committer-date cutoffs on bounded walks.
pub fn commits_reachable_excluding_ancestors_of_cutoffs(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    limit: Option<usize>,
    cutoffs: ReachableWalkCommitterCutoffs,
) -> Result<Vec<ObjectId>> {
    let walk_limit = match limit {
        Some(n) => ReachableWalkLimit::StopAfter(n),
        None => ReachableWalkLimit::Unlimited,
    };
    let (_emitted, oids) = walk_commits_reachable_excluding_ancestors_of_cutoffs(
        repo, tips, hide, walk_limit, cutoffs,
    )?;
    Ok(oids)
}

/// [`commits_reachable_excluding_ancestors_of_cutoffs`] that can skip the on-disk commit-graph
/// and parse every commit object instead.
pub(crate) fn commits_reachable_excluding_ancestors_of_with_graph(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    limit: Option<usize>,
    cutoffs: ReachableWalkCommitterCutoffs,
    use_commit_graph: bool,
) -> Result<Vec<ObjectId>> {
    if tips.is_empty() {
        return Ok(Vec::new());
    }
    ReachableWalkState::with_commit_graph(repo, tips, hide, use_commit_graph)?
        .emit_until(repo, limit, cutoffs)
}

/// Return commits that are not reachable from any other input commit.
///
/// The output order follows input order, dropping any commit reachable from
/// another supplied commit.
///
/// # Errors
///
/// Returns parse and object read errors from commit traversal.
pub fn independent_commits(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    let mut cache = CommitGraphCache::new(repo);
    let mut out = Vec::new();
    for (i, &candidate) in commits.iter().enumerate() {
        let mut reachable = false;
        for (j, &other) in commits.iter().enumerate() {
            if i == j {
                continue;
            }
            if cache.ancestor_closure(other)?.contains(&candidate) {
                reachable = true;
                break;
            }
        }
        if !reachable {
            out.push(candidate);
        }
    }
    Ok(out)
}

/// Select the best base ref for a target tip using Git's first-parent branch-base heuristic.
///
/// The returned index points into `bases`. The algorithm walks the first-parent histories of
/// `tip` and the candidate bases, picking the base whose first-parent path collides with the
/// tip's first-parent path at the newest branch point. Ties keep the earliest candidate index.
///
/// # Parameters
///
/// - `repo` - repository used to read commit parents.
/// - `tip` - target commit whose branch base is being queried.
/// - `bases` - candidate base commits in caller-visible order.
///
/// # Errors
///
/// Returns object read or parse errors for malformed commit history.
pub fn branch_base_for_tip(
    repo: &Repository,
    tip: ObjectId,
    bases: &[ObjectId],
) -> Result<Option<usize>> {
    if bases.is_empty() {
        return Ok(None);
    }

    let tip_chain = first_parent_chain(repo, tip)?;
    let tip_positions: OidHashMap<usize> = tip_chain
        .iter()
        .copied()
        .enumerate()
        .map(|(index, oid)| (oid, index))
        .collect();

    let mut best: Option<(usize, usize)> = None;
    for (base_index, &base) in bases.iter().enumerate() {
        for oid in first_parent_chain(repo, base)? {
            let Some(&tip_position) = tip_positions.get(&oid) else {
                continue;
            };
            match best {
                None => best = Some((tip_position, base_index)),
                Some((best_position, best_index))
                    if tip_position < best_position
                        || (tip_position == best_position && base_index < best_index) =>
                {
                    best = Some((tip_position, base_index));
                }
                _ => {}
            }
            break;
        }
    }

    Ok(best.map(|(_, index)| index))
}

fn first_parent_chain(repo: &Repository, start: ObjectId) -> Result<Vec<ObjectId>> {
    let mut chain = Vec::new();
    let mut current = Some(start);
    while let Some(oid) = current {
        chain.push(oid);
        current = first_parent(repo, oid)?;
    }
    Ok(chain)
}

fn first_parent(repo: &Repository, oid: ObjectId) -> Result<Option<ObjectId>> {
    let commit = repo.odb().read_commit(&oid)?;
    Ok(commit.parents.first().copied())
}

fn ensure_is_commit(repo: &Repository, oid: ObjectId) -> Result<()> {
    let object = repo.odb().read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    Ok(())
}

const UNRANKED_COMMIT: u64 = (1u64 << 63) - 1;
const LEGACY_RANK_CAP: u64 = 0x3FFF_FFFF;

const MARK_TIP_A: u8 = 1 << 0;
const MARK_TIP_B: u8 = 1 << 1;
const MARK_PRUNED: u8 = 1 << 2;
const MARK_BASE: u8 = 1 << 3;
const MARK_QUEUED: u8 = 1 << 4;

#[cfg(test)]
pub(crate) mod intersect_walk_test_hooks {
    use std::cell::Cell;

    thread_local! {
        static INTERSECT_WALK_STEPS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset_intersect_walk_steps() {
        INTERSECT_WALK_STEPS.with(|c| c.set(0));
    }

    pub fn intersect_walk_steps() -> usize {
        INTERSECT_WALK_STEPS.with(|c| c.get())
    }

    pub(crate) fn record_intersect_walk_step() {
        INTERSECT_WALK_STEPS.with(|c| c.set(c.get().saturating_add(1)));
    }

    thread_local! {
        static COMMIT_PARENT_LOOKUPS: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset_commit_parent_lookups() {
        COMMIT_PARENT_LOOKUPS.with(|c| c.set(0));
    }

    pub fn commit_parent_lookups() -> usize {
        COMMIT_PARENT_LOOKUPS.with(|c| c.get())
    }

    pub(crate) fn record_commit_parent_lookup() {
        COMMIT_PARENT_LOOKUPS.with(|c| c.set(c.get().saturating_add(1)));
    }

    thread_local! {
        /// Parent reads including cache hits: repeated walks over cached commits show here.
        static COMMIT_PARENT_QUERIES: Cell<usize> = const { Cell::new(0) };
    }

    pub fn reset_commit_parent_queries() {
        COMMIT_PARENT_QUERIES.with(|c| c.set(0));
    }

    pub fn commit_parent_queries() -> usize {
        COMMIT_PARENT_QUERIES.with(|c| c.get())
    }

    pub(crate) fn record_commit_parent_query() {
        COMMIT_PARENT_QUERIES.with(|c| c.set(c.get().saturating_add(1)));
    }
}

#[derive(Eq, PartialEq)]
struct RankedCommit {
    rank: u64,
    committed_at: i64,
    /// Queue insertion counter: among equal rank and date, the earlier queued commit pops
    /// first, so merge bases with equal dates come out in Git's discovery order.
    queued_at: u64,
    oid: ObjectId,
}

impl Ord for RankedCommit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.rank
            .cmp(&other.rank)
            .then_with(|| self.committed_at.cmp(&other.committed_at))
            .then_with(|| other.queued_at.cmp(&self.queued_at))
    }
}

impl PartialOrd for RankedCommit {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

struct OpenQueueTally {
    open_on_a: isize,
    open_on_b: isize,
    open_on_both: isize,
    newest_rank_seen: u64,
    rank_stop: u64,
    /// Commits queued so far; stamps [`RankedCommit::queued_at`].
    queued: u64,
}

impl OpenQueueTally {
    fn apply_label(&mut self, label: u8, delta: isize) {
        let side = label & (MARK_TIP_A | MARK_TIP_B | MARK_PRUNED);
        if side == MARK_TIP_A | MARK_TIP_B | MARK_PRUNED {
            return;
        }
        if side == MARK_TIP_A | MARK_TIP_B {
            self.open_on_both += delta;
        } else if side == MARK_TIP_A {
            self.open_on_a += delta;
        } else if side == MARK_TIP_B {
            self.open_on_b += delta;
        }
    }
}

fn first_parent_oid(cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> Result<ObjectId> {
    let parents = cache.parents_of(oid)?;
    parents
        .first()
        .copied()
        .ok_or_else(|| Error::ObjectNotFound(format!("{oid} has no parent")))
}

/// Classify `first` and `second` along first-parent chains, using commit-graph generations
/// to bound the walk when both commits are ranked and [`first_parent_chain_relation`]
/// otherwise.
fn direct_ancestor_relation(
    cache: &mut CommitGraphCache<'_>,
    first: ObjectId,
    second: ObjectId,
) -> Result<FirstParentRelation> {
    if first == second {
        return Ok(FirstParentRelation::Same);
    }
    let first_gen = commit_rank_for_walk(cache, first);
    let second_gen = commit_rank_for_walk(cache, second);
    if first_gen == UNRANKED_COMMIT || second_gen == UNRANKED_COMMIT {
        return Ok(first_parent_chain_relation(cache, first, second));
    }
    if first_gen == second_gen {
        return Ok(FirstParentRelation::Unrelated);
    }
    let (ancestor, ancestor_gen, mut cur, mut gen, found) = if first_gen < second_gen {
        (
            first,
            first_gen,
            second,
            second_gen,
            FirstParentRelation::FirstIsAncestor,
        )
    } else {
        (
            second,
            second_gen,
            first,
            first_gen,
            FirstParentRelation::SecondIsAncestor,
        )
    };
    while gen > ancestor_gen {
        cur = first_parent_oid(cache, cur)?;
        gen = commit_rank_for_walk(cache, cur);
        if gen == UNRANKED_COMMIT {
            return Ok(FirstParentRelation::Unrelated);
        }
    }
    Ok(if cur == ancestor {
        found
    } else {
        FirstParentRelation::Unrelated
    })
}

/// How two commits relate along their first-parent chains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FirstParentRelation {
    /// Both inputs are the same commit.
    Same,
    /// The first commit lies on the second commit's first-parent chain.
    FirstIsAncestor,
    /// The second commit lies on the first commit's first-parent chain.
    SecondIsAncestor,
    /// Neither commit lies on the other's first-parent chain (they may still be
    /// related through second parents).
    Unrelated,
}

/// Classify `first` and `second` by walking both first-parent chains together.
///
/// Stops as soon as one chain reaches the other tip, or the chains merge at a commit that
/// is neither tip: once merged they share every later commit, so the tip that is not yet
/// on the other chain can never appear on it. That argument holds for any interleaving of
/// the two walks, so committer dates only pick which chain to advance (newer first) and
/// never affect the answer. With sane dates the cost is about the distance to the answer
/// instead of a walk to the root of the losing direction. A root commit or a
/// missing or unreadable parent ends that chain; the caller's full walk reports real errors.
fn first_parent_chain_relation(
    cache: &mut CommitGraphCache<'_>,
    first: ObjectId,
    second: ObjectId,
) -> FirstParentRelation {
    if first == second {
        return FirstParentRelation::Same;
    }
    let mut seen_first = oid_hash_set();
    seen_first.insert(first);
    let mut seen_second = oid_hash_set();
    seen_second.insert(second);
    let mut cur_first = Some(first);
    let mut cur_second = Some(second);
    while cur_first.is_some() || cur_second.is_some() {
        // Advance the newer chain first so a descendant reaches its ancestor without also
        // walking the ancestor's chain; equal or unreadable dates step both.
        let first_time = cur_first.and_then(|oid| cache.commit_time(oid).ok());
        let second_time = cur_second.and_then(|oid| cache.commit_time(oid).ok());
        let (step_first, step_second) = match (cur_first, cur_second) {
            (Some(_), Some(_)) => match (first_time, second_time) {
                (Some(a), Some(b)) if a > b => (true, false),
                (Some(a), Some(b)) if a < b => (false, true),
                _ => (true, true),
            },
            (first, second) => (first.is_some(), second.is_some()),
        };
        if let Some(cur) = cur_first.filter(|_| step_first) {
            cur_first = first_parent_oid(cache, cur).ok();
            if let Some(next) = cur_first {
                if next == second {
                    return FirstParentRelation::SecondIsAncestor;
                }
                if seen_second.contains(&next) {
                    return FirstParentRelation::Unrelated;
                }
                seen_first.insert(next);
            }
        }
        if let Some(cur) = cur_second.filter(|_| step_second) {
            cur_second = first_parent_oid(cache, cur).ok();
            if let Some(next) = cur_second {
                if next == first {
                    return FirstParentRelation::FirstIsAncestor;
                }
                if seen_first.contains(&next) {
                    return FirstParentRelation::Unrelated;
                }
                seen_second.insert(next);
            }
        }
    }
    FirstParentRelation::Unrelated
}

/// Commit-graph generation when present (`None` if the commit is outside the graph).
fn commit_graph_rank(cache: &CommitGraphCache<'_>, oid: ObjectId) -> Option<u64> {
    let graph = cache.graph.as_ref()?;
    let rank = graph.commit_generation_for_walk(&oid);
    if rank == UNRANKED_COMMIT {
        None
    } else {
        Some(rank)
    }
}

fn commit_rank_for_walk(cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> u64 {
    if let Some(rank) = commit_graph_rank(cache, oid) {
        return rank;
    }
    UNRANKED_COMMIT
}

/// Heap ordering key: commit-graph generation when present, else [`UNRANKED_COMMIT`]
/// with [`CommitGraphCache::commit_time`] as the tie-break (see [`PaintKey`]).
fn push_commit_heap_key(
    cache: &mut CommitGraphCache<'_>,
    heap: &mut BinaryHeap<PaintKey>,
    seq: &mut u64,
    oid: ObjectId,
) -> Result<()> {
    let rank = commit_rank_for_walk(cache, oid);
    let committed_at = cache.commit_time(oid)?;
    heap.push(PaintKey {
        rank,
        committed_at,
        seq: std::cmp::Reverse(*seq),
        oid,
    });
    *seq += 1;
    Ok(())
}

/// Commit is reachable from the left tips of a [`SidePaint`].
const PAINT_LEFT: u8 = 1;
/// Commit is reachable from the right tips of a [`SidePaint`].
const PAINT_RIGHT: u8 = 2;
const PAINT_BOTH: u8 = PAINT_LEFT | PAINT_RIGHT;

/// Two-sided reachability paint (Git's `ahead_behind` walk).
///
/// One walk from both sides at once, newest generation (then committer date)
/// first, marking each commit with the sides that reach it. The walk stops once
/// every queued commit is reachable from both sides: everything below that
/// frontier is common history. Commits never painted are therefore reachable
/// from both sides (or from neither).
///
/// With commit-graph generations the result is exact; for commits outside the
/// graph, ordering falls back to committer dates like Git does.
#[derive(Clone, Debug, Default)]
struct SidePaint {
    sides: OidHashMap<u8>,
    /// The paint stopped at a commit reachable from the left side alone dated outside its
    /// [`LeftDateWindow`], so `sides` is incomplete.
    left_outside_window: bool,
}

impl SidePaint {
    /// Paint ancestors of `left` and `right` down to their common frontier.
    ///
    /// # Errors
    ///
    /// Propagates commit read and parse errors from the walk.
    fn compute(
        cache: &mut CommitGraphCache<'_>,
        left: &[ObjectId],
        right: &[ObjectId],
    ) -> Result<Self> {
        Self::paint(cache, left, right, PaintWalk::default())
    }

    /// Paint `tips` against the `hide` tips of a range walk. Like Git's walk over excluded
    /// commits, a missing object reached from `hide` is skipped rather than an error: it is
    /// hidden whatever lies behind it.
    ///
    /// # Errors
    ///
    /// Propagates commit read and parse errors for commits not reached from `hide`.
    fn compute_hiding(
        cache: &mut CommitGraphCache<'_>,
        tips: &[ObjectId],
        hide: &[ObjectId],
        stop: HidePaintStop,
        left_window: Option<LeftDateWindow>,
    ) -> Result<Self> {
        let walk = PaintWalk {
            left_window,
            skip_missing_right: true,
            open_while_left_only: matches!(
                stop,
                HidePaintStop::TipsResolved | HidePaintStop::TipsResolvedDateSlop
            ),
            date_slop: stop == HidePaintStop::TipsResolvedDateSlop,
            ..PaintWalk::default()
        };
        Self::paint(cache, tips, hide, walk)
    }

    /// [`Self::compute_hiding`] with [`HidePaintStop::TipsResolved`] for a `--first-parent`
    /// walk: the left side follows only first parents, while `hide` still hides everything it
    /// reaches through any parent, as in Git. Only commits on the tips' first-parent chains are
    /// painted left, so the walk ends where those chains meet hidden history.
    ///
    /// # Errors
    ///
    /// Propagates commit read and parse errors for commits not reached from `hide`.
    fn compute_hiding_first_parent(
        cache: &mut CommitGraphCache<'_>,
        tips: &[ObjectId],
        hide: &[ObjectId],
    ) -> Result<Self> {
        let walk = PaintWalk {
            skip_missing_right: true,
            open_while_left_only: true,
            left_first_parent_only: true,
            ..PaintWalk::default()
        };
        Self::paint(cache, tips, hide, walk)
    }

    fn paint(
        cache: &mut CommitGraphCache<'_>,
        left: &[ObjectId],
        right: &[ObjectId],
        mut walk: PaintWalk,
    ) -> Result<Self> {
        cache.prime_tips(left);
        cache.prime_tips(right);
        for (tips, side) in [(left, PAINT_LEFT), (right, PAINT_RIGHT)] {
            for &tip in tips {
                let Some(tip) = cache.peel_tip(tip)? else {
                    continue;
                };
                walk.mark(cache, tip, side)?;
            }
        }
        let mut slop = PAINT_SLOP;
        loop {
            if walk.open > 0 {
                slop = PAINT_SLOP;
            } else if !walk.unranked_hide_may_reach_left(&mut slop) {
                break;
            }
            let Some(node) = walk.heap.pop() else {
                break;
            };
            if walk.queued.remove(&node.oid) == Some(true) {
                walk.open -= 1;
            }
            walk.popped.insert(node.oid);
            let side = walk.sides.get(&node.oid).copied().unwrap_or(0);
            if side == PAINT_LEFT
                && walk
                    .left_window
                    .is_some_and(|window| !window.contains(node.committed_at))
            {
                return Ok(Self {
                    sides: walk.sides,
                    left_outside_window: true,
                });
            }
            if walk.date_slop && side == PAINT_LEFT && node.rank == UNRANKED_COMMIT {
                walk.left_floor = Some(
                    walk.left_floor
                        .map_or(node.committed_at, |floor| floor.min(node.committed_at)),
                );
            }
            for (i, parent) in cache.parents_of(node.oid)?.into_iter().enumerate() {
                let side = if i > 0 && walk.left_first_parent_only {
                    side & PAINT_RIGHT
                } else {
                    side
                };
                walk.mark(cache, parent, side)?;
            }
        }
        Ok(Self {
            sides: walk.sides,
            left_outside_window: false,
        })
    }

    /// Number of painted commits reachable from `side` only.
    fn count_only(&self, side: u8) -> usize {
        self.sides.values().filter(|&&s| s == side).count()
    }

    /// The same paint with left and right exchanged, for walking from the right tips.
    fn swapped(&self) -> Self {
        let sides = self
            .sides
            .iter()
            .map(|(&oid, &s)| {
                let flipped = match s {
                    PAINT_LEFT => PAINT_RIGHT,
                    PAINT_RIGHT => PAINT_LEFT,
                    other => other,
                };
                (oid, flipped)
            })
            .collect();
        Self {
            sides,
            left_outside_window: self.left_outside_window,
        }
    }

    /// Whether `side` reaches `oid`, or [`None`] when the walk never painted it.
    fn reached_by(&self, oid: ObjectId, side: u8) -> Option<bool> {
        self.sides.get(&oid).map(|s| s & side != 0)
    }
}

/// Queue state for [`SidePaint::compute`].
#[derive(Default)]
struct PaintWalk {
    sides: OidHashMap<u8>,
    /// Queued commits, each flagged whether it keeps the walk going.
    queued: OidHashMap<bool>,
    popped: OidHashSet,
    heap: BinaryHeap<PaintKey>,
    seq: u64,
    /// Queued commits that still keep the walk going (see [`Self::mark`]).
    open: usize,
    /// Leave unreadable commits reached from the right side unqueued instead of failing.
    skip_missing_right: bool,
    /// Commits with a commit-graph generation keep the walk going only while the left side
    /// alone reaches them ([`HidePaintStop::TipsResolved`]).
    open_while_left_only: bool,
    /// Commits without a generation follow the same rule, and the walk ends by committer date
    /// ([`HidePaintStop::TipsResolvedDateSlop`]).
    date_slop: bool,
    /// Oldest committer date among commits without a generation popped while the left side
    /// alone reached them.
    left_floor: Option<i64>,
    /// Pass the left paint to first parents only ([`SidePaint::compute_hiding_first_parent`]).
    left_first_parent_only: bool,
    /// Stop when a commit popped as reachable from the left side alone is dated outside it.
    left_window: Option<LeftDateWindow>,
}

/// Commits a [`HidePaintStop::TipsResolvedDateSlop`] paint pops past its date guard, so a hide
/// commit dated slightly too old still repaints the ancestor it reaches. Git's `rev-list` walks
/// the same number of commits past its "everything left is uninteresting" point.
const PAINT_SLOP: usize = 5;

/// How much older than a popped left-only commit a queued hide commit without a generation may
/// be dated and still keep a [`HidePaintStop::TipsResolvedDateSlop`] paint going: the clock
/// skew it tolerates exactly.
const UNRANKED_SKEW_ALLOWANCE: time::Duration = time::Duration::DAY;

impl PaintWalk {
    /// Date-slop paint with nothing left-only queued: whether a queued commit without a
    /// generation may still reach a commit popped as left-only, so the walk goes on.
    ///
    /// A commit with a generation cannot: the graph holds every ancestor of its commits, and
    /// those all sort below commits outside it. Otherwise the walk continues while the next
    /// queued commit is dated within [`UNRANKED_SKEW_ALLOWANCE`] of the oldest left-only commit
    /// popped, then for [`PAINT_SLOP`] more commits.
    fn unranked_hide_may_reach_left(&self, slop: &mut usize) -> bool {
        if !self.date_slop {
            return false;
        }
        let (Some(floor), Some(next)) = (self.left_floor, self.heap.peek()) else {
            return false;
        };
        if next.rank != UNRANKED_COMMIT {
            return false;
        }
        let horizon = floor.saturating_sub(UNRANKED_SKEW_ALLOWANCE.whole_seconds());
        if next.committed_at >= horizon {
            return true;
        }
        if *slop == 0 {
            return false;
        }
        *slop -= 1;
        true
    }

    /// Add `side` to `oid`, queueing it when its paint grows.
    ///
    /// A queued commit keeps the walk going while it is not reachable from both
    /// sides, or when it was already popped: a commit popped before the other
    /// side reached it (equal or skewed dates) has to pass the new side on to
    /// the ancestors it already painted.
    fn mark(&mut self, cache: &mut CommitGraphCache<'_>, oid: ObjectId, side: u8) -> Result<()> {
        let before = self.sides.get(&oid).copied().unwrap_or(0);
        let after = before | side;
        if after == before {
            return Ok(());
        }
        self.sides.insert(oid, after);
        let repaint = self.popped.contains(&oid);
        if let Some(is_open) = self.queued.get_mut(&oid) {
            if *is_open && after == PAINT_BOTH && !repaint {
                *is_open = false;
                self.open -= 1;
            }
            return Ok(());
        }
        let committed_at = match cache.commit_time(oid) {
            Ok(time) => time,
            // Hidden either way, and unpainted ancestors count as hidden too.
            Err(Error::ObjectNotFound(_))
                if self.skip_missing_right && after & PAINT_RIGHT != 0 =>
            {
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        let rank = commit_rank_for_walk(cache, oid);
        let unresolved = if self.open_while_left_only && (rank != UNRANKED_COMMIT || self.date_slop)
        {
            after == PAINT_LEFT
        } else {
            after != PAINT_BOTH
        };
        let is_open = unresolved || repaint;
        self.queued.insert(oid, is_open);
        if is_open {
            self.open += 1;
        }
        self.heap.push(PaintKey {
            rank,
            committed_at,
            seq: std::cmp::Reverse(self.seq),
            oid,
        });
        self.seq += 1;
        Ok(())
    }
}

/// [`PaintWalk`] heap order: highest generation, then newest committer date,
/// then first queued (Git's priority queue is stable for ties).
#[derive(Eq, Ord, PartialEq, PartialOrd)]
struct PaintKey {
    rank: u64,
    committed_at: i64,
    seq: std::cmp::Reverse<u64>,
    oid: ObjectId,
}

/// Newest committer timestamp first, matching `git merge-base` stdout ordering.
fn sort_merge_bases_for_git_output(
    cache: &mut CommitGraphCache<'_>,
    bases: &mut [ObjectId],
) -> Result<()> {
    let mut keyed = Vec::with_capacity(bases.len());
    for &oid in bases.iter() {
        keyed.push((cache.commit_time(oid)?, oid));
    }
    keyed.sort_by_key(|a| std::cmp::Reverse(a.0));
    for (slot, (_, oid)) in keyed.into_iter().enumerate() {
        bases[slot] = oid;
    }
    Ok(())
}

fn enqueue_commit_for_intersect_walk(
    cache: &mut CommitGraphCache<'_>,
    pending: &mut BinaryHeap<RankedCommit>,
    tally: &mut OpenQueueTally,
    labels: &mut OidHashMap<u8>,
    oid: ObjectId,
    extra: u8,
) -> Result<()> {
    let slot = labels.entry(oid).or_insert(0);
    let before = *slot;
    let merged = before | extra;
    *slot = merged;
    if before & MARK_QUEUED != 0 {
        tally.apply_label(before, -1);
        tally.apply_label(merged, 1);
        return Ok(());
    }
    *slot = merged | MARK_QUEUED;
    let rank = commit_rank_for_walk(cache, oid);
    let committed_at = cache.commit_time(oid)?;
    tally.queued += 1;
    pending.push(RankedCommit {
        rank,
        committed_at,
        queued_at: tally.queued,
        oid,
    });
    tally.apply_label(merged | MARK_QUEUED, 1);
    Ok(())
}

fn dequeue_commit_for_intersect_walk(
    pending: &mut BinaryHeap<RankedCommit>,
    tally: &mut OpenQueueTally,
    labels: &mut OidHashMap<u8>,
    allow_rank_stop: bool,
) -> Option<ObjectId> {
    let node = pending.pop()?;
    tally.newest_rank_seen = node.rank;
    let before = labels.get(&node.oid).copied().unwrap_or(0);
    labels.insert(node.oid, before & !MARK_QUEUED);
    if tally.open_on_both == 0 {
        if tally.open_on_a == 0 && tally.open_on_b == 0 {
            // Queue holds only stale commits (the popped one included): no new merge-base
            // candidates can appear, and walking on would follow the stale tail to the root.
            // `reduce_to_best` removes redundant bases afterward (Git paint_down_to_common).
            return None;
        } else if allow_rank_stop
            && (tally.open_on_a == 0 || tally.open_on_b == 0)
            && node.rank != UNRANKED_COMMIT
            && node.rank < tally.rank_stop
        {
            return None;
        }
    }
    tally.apply_label(before, -1);
    Some(node.oid)
}

/// Walk two histories toward their intersection; optionally collect every minimal base.
fn walk_intersecting_histories(
    cache: &mut CommitGraphCache<'_>,
    tip_a: ObjectId,
    tips_b: &[ObjectId],
    collect_every_base: bool,
) -> Result<Vec<ObjectId>> {
    let mut pending = BinaryHeap::new();
    let mut labels: OidHashMap<u8> = oid_hash_map();
    let mut tally = OpenQueueTally {
        open_on_a: 0,
        open_on_b: 0,
        open_on_both: 0,
        newest_rank_seen: UNRANKED_COMMIT,
        rank_stop: LEGACY_RANK_CAP,
        queued: 0,
    };
    let mut discovered: Vec<ObjectId> = Vec::new();

    labels.insert(tip_a, MARK_TIP_A);
    enqueue_commit_for_intersect_walk(cache, &mut pending, &mut tally, &mut labels, tip_a, 0)?;
    for &tip_b in tips_b {
        enqueue_commit_for_intersect_walk(
            cache,
            &mut pending,
            &mut tally,
            &mut labels,
            tip_b,
            MARK_TIP_B,
        )?;
    }

    while let Some(oid) = dequeue_commit_for_intersect_walk(
        &mut pending,
        &mut tally,
        &mut labels,
        !collect_every_base,
    ) {
        #[cfg(test)]
        intersect_walk_test_hooks::record_intersect_walk_step();
        let mut propagate =
            labels.get(&oid).copied().unwrap_or(0) & (MARK_TIP_A | MARK_TIP_B | MARK_PRUNED);
        if propagate == MARK_TIP_A | MARK_TIP_B {
            let slot = labels.entry(oid).or_insert(0);
            if *slot & MARK_BASE == 0 {
                *slot |= MARK_BASE;
                discovered.push(oid);
                if !collect_every_base && tally.newest_rank_seen < tally.rank_stop {
                    break;
                }
            }
            propagate |= MARK_PRUNED;
        }
        for parent in cache.parents_of(oid)? {
            let have = labels.get(&parent).copied().unwrap_or(0);
            if (have & propagate) == propagate {
                continue;
            }
            enqueue_commit_for_intersect_walk(
                cache,
                &mut pending,
                &mut tally,
                &mut labels,
                parent,
                propagate,
            )?;
        }
    }

    let mut bases = Vec::new();
    for oid in discovered {
        if labels.get(&oid).copied().unwrap_or(0) & MARK_PRUNED == 0 {
            bases.push(oid);
        }
    }
    if !collect_every_base && !bases.is_empty() {
        let candidates: OidHashSet = bases.into_iter().collect();
        bases = reduce_to_best(candidates, cache)?;
    }
    sort_merge_bases_for_git_output(cache, &mut bases)?;
    Ok(bases)
}

fn reduce_to_best(
    candidates: OidHashSet,
    cache: &mut CommitGraphCache<'_>,
) -> Result<Vec<ObjectId>> {
    if candidates.is_empty() {
        return Ok(Vec::new());
    }
    let mut best = BTreeSet::new();
    for &candidate in &candidates {
        let mut better_found = false;
        for &other in &candidates {
            if candidate == other {
                continue;
            }
            if cache.is_ancestor(candidate, other)? {
                better_found = true;
                break;
            }
        }
        if !better_found {
            best.insert(candidate);
        }
    }
    Ok(best.into_iter().collect())
}

/// Fewest tips on one side of a paint for [`CommitGraphCache::prime_tips`] to read them up
/// front; merge-base and range walks start from one or two tips and keep the plain reads.
const PRIME_TIPS_MIN: usize = 64;

struct CommitGraphCache<'r> {
    repo: &'r Repository,
    closures: OidHashMap<OidHashSet>,
    promisor_stop: OidHashSet,
    shallow_boundaries: OidHashSet,
    /// Parent overrides from `.git/info/grafts` (same as Git and [`crate::rev_list::CommitGraph`]).
    graft_parents: OidHashMap<Vec<ObjectId>>,
    /// Commit-graph file, when present: supplies parents + committer time without
    /// decompressing commit objects. Commits absent from it (or octopus merges)
    /// fall back to [`Self::parents_of`]'s object read.
    graph: Option<crate::commit_graph_file::CommitGraphChain>,
    /// Monotonic pack cursor for single-pack repos (deep-history benchmarks).
    single_pack: Option<crate::pack::SingleLocalPackReader>,
    /// Set when [`Self::single_pack`] could not be opened under a matching pack-read context.
    single_pack_unavailable: bool,
    /// When set, the commit objects [`Self::commit_time_for_heap`] reads from the object
    /// database, so a caller that renders the walked commits does not read them a second time.
    retained: Option<WalkedCommits>,
    /// True only while [`Self::commit_time_for_heap`] runs, so commits read for painting
    /// or ancestry checks are not retained.
    retain_reads: bool,
    /// Payload of the commit [`Self::parents_of`] parses, reused across reads. Linkage and
    /// [`Self::retained`] keep what the walk needs, so a per-commit allocation (and the
    /// object database's session copy of every walked commit) would only be thrown away.
    read_buf: Vec<u8>,
    /// Plain commits whose linkage [`Self::prime_tips`] recorded from their header blocks.
    header_primed: OidHashSet,
}

impl<'r> CommitGraphCache<'r> {
    fn new(repo: &'r Repository) -> Self {
        Self::with_commit_graph(repo, true)
    }

    /// `use_commit_graph: false` leaves [`Self::graph`] empty so every lookup parses the commit.
    fn with_commit_graph(repo: &'r Repository, use_commit_graph: bool) -> Self {
        let cfg = repo.config().unwrap_or_default();
        // Stop ancestry traversal only at genuinely-missing promisor objects, not
        // at every member of a promisor pack. The clone base commit lives in a
        // promisor pack but is fully present locally; treating it as a stop point
        // truncates ancestry and breaks fast-forward detection on a push from a
        // partial clone (t5616 "after fetching descendants of non-promisor
        // commits, gc works"). Missing parents are already handled by
        // `parents_of` returning no parents on `ObjectNotFound`, so we only need
        // to record the OIDs the partial clone knows are absent.
        let promisor_stop = if repo_treats_promisor_packs(repo.git_dir(), &cfg) {
            read_promisor_missing_oids(repo.git_dir())
                .into_iter()
                .collect::<OidHashSet>()
        } else {
            oid_hash_set()
        };
        let graph = if use_commit_graph {
            repo.odb().commit_graph_chain().map(|arc| (*arc).clone())
        } else {
            None
        };
        let shallow_boundaries = oid_hash_set_from(&load_shallow_boundaries(repo.git_dir()));
        let graft_parents = load_graft_parents(repo.git_dir()).into_iter().collect();
        Self {
            repo,
            closures: oid_hash_map(),
            promisor_stop,
            shallow_boundaries,
            graft_parents,
            graph,
            single_pack: None,
            single_pack_unavailable: false,
            retained: None,
            retain_reads: false,
            read_buf: Vec::new(),
            header_primed: oid_hash_set(),
        }
    }

    /// Read `oid` into `buf` and return its kind. Delta bases stay cached, as for
    /// [`crate::odb::Odb::read`].
    ///
    /// The single local pack answers first; an object it does not hold (one in an alternate,
    /// say) is read through the object database, so a walk never mistakes it for missing.
    fn read_object_into(&mut self, oid: &ObjectId, buf: &mut Vec<u8>) -> Result<ObjectKind> {
        self.ensure_single_pack_reader();
        if let Some(reader) = self.single_pack.as_mut() {
            match reader.read_object_into_with_delta_policy(oid, true, buf) {
                Err(Error::ObjectNotFound(_)) => {}
                read => return read,
            }
        }
        self.repo.odb().read_into_caching_delta_bases(oid, buf)
    }

    /// Parent rewrites can invalidate commit-graph linkage and create ancestry cycles.
    fn has_parent_rewrites(&self) -> Result<bool> {
        if !self.graft_parents.is_empty() {
            return Ok(true);
        }
        let mut has_replacements = false;
        if let Some(prefix) = self.repo.replace_ref_base_if_enabled() {
            self.repo.refs().for_each_ref(&prefix, &mut |_| {
                has_replacements = true;
                std::ops::ControlFlow::Break(())
            })?;
        }
        Ok(has_replacements)
    }

    fn grafted_parents(&self, oid: ObjectId, parents: Vec<ObjectId>) -> Vec<ObjectId> {
        self.graft_parents.get(&oid).cloned().unwrap_or(parents)
    }

    fn with_linkage<R>(
        &self,
        f: impl FnOnce(&mut crate::repo_caches::CommitLinkageCache) -> R,
    ) -> R {
        let mut guard = self
            .repo
            .caches()
            .commit_linkage()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        f(&mut guard)
    }

    /// Open the reader over the single local pack, once per pack-read context.
    ///
    /// Loose objects may exist beside the pack: [`Self::read_object_into`] reads whatever the
    /// pack lacks through the object database.
    fn ensure_single_pack_reader(&mut self) {
        if self.single_pack.is_some()
            || self.single_pack_unavailable
            || !self.repo.odb().uses_files_primary()
        {
            return;
        }
        let objects_dir = self.repo.odb().objects_dir();
        if !crate::pack_store::PackStore::read_context_matches_objects_dir(objects_dir) {
            return;
        }
        match crate::pack::SingleLocalPackReader::open_allow_loose(objects_dir) {
            Ok(Some(reader)) => self.single_pack = Some(reader),
            // Several packs, or none: the listing will not change during this walk.
            _ => self.single_pack_unavailable = true,
        }
    }

    fn ancestor_closure(&mut self, start: ObjectId) -> Result<OidHashSet> {
        if let Some(existing) = self.closures.get(&start) {
            return Ok(existing.clone());
        }

        let mut visited = oid_hash_set();
        let mut queue = VecDeque::new();
        queue.push_back(start);
        while let Some(oid) = queue.pop_front() {
            if !visited.insert(oid) {
                continue;
            }
            for parent in self.parents_of(oid)? {
                queue.push_back(parent);
            }
        }
        self.closures.insert(start, visited.clone());
        Ok(visited)
    }

    fn parents_of(&mut self, oid: ObjectId) -> Result<Vec<ObjectId>> {
        #[cfg(test)]
        intersect_walk_test_hooks::record_commit_parent_query();
        if let Some(parents) = self.with_linkage(|link| link.parents.get(&oid).cloned()) {
            return Ok(parents);
        }
        #[cfg(test)]
        intersect_walk_test_hooks::record_commit_parent_lookup();
        if self.promisor_stop.contains(&oid) {
            let parents = self.grafted_parents(oid, Vec::new());
            self.with_linkage(|link| {
                link.parents.insert(oid, parents.clone());
                link.times.insert(oid, 0);
            });
            return Ok(parents);
        }
        if self.shallow_boundaries.contains(&oid) {
            let commit_oid =
                peel_to_commit_for_merge_base(self.repo, oid).map_err(|e| match e {
                    Error::InvalidRef(msg) => Error::CorruptObject(msg),
                    other => other,
                })?;
            let object = self.repo.odb().read(&commit_oid)?;
            if object.kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            let commit = CommitRef::parse(&object.data)?;
            let time = commit.committer_timestamp();
            let parents = self.grafted_parents(oid, Vec::new());
            self.with_linkage(|link| {
                link.times.insert(oid, time);
                link.parents.insert(oid, parents.clone());
            });
            return Ok(parents);
        }
        // Fast path: parents + committer time from the commit-graph file (no
        // object decompression). Misses (not in graph / octopus) fall through.
        if let Some(graph) = &self.graph {
            if let Some((parents, ctime)) = graph.graph_commit(&oid) {
                let parents = if self.shallow_boundaries.contains(&oid) {
                    Vec::new()
                } else {
                    parents
                        .into_iter()
                        .filter(|p| !self.promisor_stop.contains(p))
                        .collect()
                };
                let parents = self.grafted_parents(oid, parents);
                self.with_linkage(|link| {
                    link.times.insert(oid, ctime);
                    link.parents.insert(oid, parents.clone());
                });
                return Ok(parents);
            }
        }
        // A plain commit without a replacement is its own peel target, so one read serves both
        // the peel and the parse; anything else (tags, replaced objects) peels first.
        let mut buf = std::mem::take(&mut self.read_buf);
        let parents = self.parents_from_object(oid, &mut buf);
        self.read_buf = buf;
        parents
    }

    /// [`Self::parents_of`] for a commit the commit-graph does not cover: read and parse its
    /// object through `buf`, then record its linkage.
    fn parents_from_object(&mut self, oid: ObjectId, buf: &mut Vec<u8>) -> Result<Vec<ObjectId>> {
        let effective_oid = self.repo.object_id_after_replace(&oid);
        let direct_commit =
            effective_oid == oid && self.read_object_into(&oid, buf)? == ObjectKind::Commit;
        let commit_oid = if direct_commit {
            oid
        } else {
            let commit_oid =
                peel_to_commit_for_merge_base(self.repo, oid).map_err(|e| match e {
                    Error::InvalidRef(msg) => Error::CorruptObject(msg),
                    other => other,
                })?;
            let kind =
                self.read_object_into(&self.repo.object_id_after_replace(&commit_oid), buf)?;
            if kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            commit_oid
        };
        let linkage = CommitRef::parse(buf)?;
        let (time, parsed_parents) = (linkage.committer_timestamp(), linkage.parents().to_vec());
        if self.retain_reads && effective_oid == oid && commit_oid == oid {
            if let Some(retained) = self.retained.as_mut() {
                retained.insert(oid, buf);
            }
        }
        Ok(self.record_linkage(oid, commit_oid, time, &parsed_parents))
    }

    /// Record `oid`'s committer time and its parents, read from `commit_oid` (the commit
    /// `oid` peels to), after shallow, promisor and graft rewrites. Returns those parents.
    fn record_linkage(
        &mut self,
        oid: ObjectId,
        commit_oid: ObjectId,
        time: i64,
        parsed_parents: &[ObjectId],
    ) -> Vec<ObjectId> {
        let parents: Vec<ObjectId> = if self.shallow_boundaries.contains(&commit_oid) {
            Vec::new()
        } else {
            parsed_parents
                .iter()
                .copied()
                .filter(|p| !self.promisor_stop.contains(p))
                .collect()
        };
        let parents = self.grafted_parents(oid, parents);
        self.with_linkage(|link| {
            link.times.insert(oid, time);
            link.parents.insert(oid, parents.clone());
        });
        parents
    }

    /// Whether `oid` names itself: no replace ref, promisor stop or shallow boundary rewrites it.
    fn is_plain_tip(&self, oid: &ObjectId) -> bool {
        self.repo.object_id_after_replace(oid) == *oid
            && !self.promisor_stop.contains(oid)
            && !self.shallow_boundaries.contains(oid)
    }

    fn graph_lists(&self, oid: &ObjectId) -> bool {
        self.graph
            .as_ref()
            .is_some_and(|graph| graph.find_commit(oid).is_some())
    }

    /// Record the linkage of every plain packed commit among `tips` from its header block
    /// ([`crate::odb::Odb::read_packed_commit_header_block_into`]), reading them in parallel.
    ///
    /// Hide sets hold every ref in the repository, so a walk can start from hundreds of
    /// scattered tips whose dates it needs before taking a single step. Their reads are
    /// independent, and each one only needs the headers. Commits the commit-graph lists need
    /// no read. Tips this cannot prime (tags, loose or deltified commits, unparsable headers,
    /// read errors) are left to [`Self::peel_tip`] and the regular object reads, which report
    /// any error. Sides with fewer than [`PRIME_TIPS_MIN`] tips are left alone too.
    fn prime_tips(&mut self, tips: &[ObjectId]) {
        if tips.len() < PRIME_TIPS_MIN {
            return;
        }
        let pending: Vec<ObjectId> = tips
            .iter()
            .filter(|oid| {
                !self.header_primed.contains(*oid)
                    && self.is_plain_tip(oid)
                    && !self.graph_lists(oid)
            })
            .copied()
            .collect();
        let repo = self.repo;
        let read_linkage = |oid: &ObjectId| -> Option<(i64, Vec<ObjectId>)> {
            let mut buf = Vec::new();
            if !repo
                .odb()
                .read_packed_commit_header_block_into(oid, &mut buf)
                .ok()?
            {
                return None;
            }
            let commit = CommitRef::parse(&buf).ok()?;
            Some((commit.committer_timestamp(), commit.parents().to_vec()))
        };
        let read = crate::rev_list::map_commits_in_parallel(&pending, read_linkage);
        for (oid, linkage) in pending.into_iter().zip(read) {
            if let Some((time, parents)) = linkage {
                self.record_linkage(oid, oid, time, &parents);
                self.header_primed.insert(oid);
            }
        }
    }

    /// Peel a walk tip to its commit, like [`try_peel_to_commit_for_merge_base`], without a read
    /// for tips [`Self::prime_tips`] recorded or the commit-graph lists.
    fn peel_tip(&mut self, oid: ObjectId) -> Result<Option<ObjectId>> {
        if self.header_primed.contains(&oid) || (self.is_plain_tip(&oid) && self.graph_lists(&oid))
        {
            return Ok(Some(oid));
        }
        try_peel_to_commit_for_merge_base(self.repo, oid)
    }

    /// Committer timestamp (unix seconds) for `oid`, peeling tags to their commit.
    /// Populated as a side effect of [`Self::parents_of`]; missing/unreadable
    /// objects report `0` (treated as oldest, so they prune away).
    fn commit_time(&mut self, oid: ObjectId) -> Result<i64> {
        if let Some(t) = self.with_linkage(|link| link.times.get(&oid).copied()) {
            return Ok(t);
        }
        self.parents_of(oid)?;
        Ok(self
            .with_linkage(|link| link.times.get(&oid).copied())
            .unwrap_or(0))
    }

    /// [`Self::commit_time`] for a commit being queued for emission, retaining its parsed
    /// object when retention is enabled.
    fn commit_time_for_heap(&mut self, oid: ObjectId) -> Result<i64> {
        self.retain_reads = self.retained.is_some();
        let time = self.commit_time(oid);
        self.retain_reads = false;
        time
    }

    /// Whether `ancestor` is an ancestor of (or equal to) `descendant`.
    ///
    /// Walks parents from `descendant` newest-first (a date-ordered heap),
    /// returning as soon as `ancestor` is reached instead of materialising the
    /// full ancestor closure. Once the frontier drops below `ancestor`'s commit
    /// date we keep going for a small slop window (tolerating non-monotonic
    /// committer dates / clock skew, like Git's `paint_down_to_common`) and then
    /// stop — bounding the work near the merge base rather than walking all of
    /// history.
    fn is_ancestor(&mut self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        use std::collections::BinaryHeap;

        // Compare against commit OIDs (the form the walk yields), peeling tags.
        let ancestor = peel_to_commit_for_merge_base(self.repo, ancestor).unwrap_or(ancestor);
        let descendant = peel_to_commit_for_merge_base(self.repo, descendant).unwrap_or(descendant);
        if ancestor == descendant {
            return Ok(true);
        }

        let a_time = self.commit_time(ancestor)?;

        // Tolerate up to this many commits below the cutoff before concluding the
        // ancestor is unreachable; absorbs realistic clock skew without walking
        // the whole graph.
        const SLOP: i32 = 100;
        let mut slop = SLOP;

        let mut heap: BinaryHeap<(i64, ObjectId)> = BinaryHeap::new();
        let mut visited: OidHashSet = oid_hash_set();
        let d_time = self.commit_time(descendant)?;
        visited.insert(descendant);
        heap.push((d_time, descendant));

        while let Some((time, oid)) = heap.pop() {
            if oid == ancestor {
                return Ok(true);
            }
            if time < a_time {
                slop -= 1;
                if slop <= 0 {
                    return Ok(false);
                }
            } else {
                slop = SLOP;
            }
            for parent in self.parents_of(oid)? {
                if visited.insert(parent) {
                    let pt = self.commit_time(parent)?;
                    heap.push((pt, parent));
                }
            }
        }
        Ok(false)
    }

    /// Mark branch tips that are ancestors of (merged into) any commit in `bases`.
    ///
    /// Uses commit-graph generations to stop the walk below the minimum generation among
    /// `tips`, matching Git's `tips_reachable_from_bases` / `--merged` batch filter.
    /// `bases` and `tips` must already be commit ids; they are not peeled again.
    fn tips_merged_from_bases(
        &mut self,
        bases: &[ObjectId],
        tips: &[ObjectId],
    ) -> Result<OidHashSet> {
        let mut merged = oid_hash_set();
        if bases.is_empty() || tips.is_empty() {
            return Ok(merged);
        }

        let mut tip_gens: Vec<(ObjectId, u64)> = tips
            .iter()
            .map(|&tip| (tip, commit_rank_for_walk(self, tip)))
            .collect();
        tip_gens.sort_by_key(|(_, gen)| *gen);
        let mut min_gen_index = 0usize;
        let mut min_generation = tip_gens[0].1;

        let tip_set: OidHashSet = tips.iter().copied().collect();

        let mut stack: Vec<ObjectId> = bases.to_vec();

        let mut seen: OidHashSet = oid_hash_set();
        while let Some(oid) = stack.pop() {
            if !seen.insert(oid) {
                continue;
            }
            if tip_set.contains(&oid) {
                merged.insert(oid);
                while min_gen_index < tip_gens.len() && merged.contains(&tip_gens[min_gen_index].0)
                {
                    min_gen_index += 1;
                }
                if min_gen_index >= tip_gens.len() {
                    break;
                }
                min_generation = tip_gens[min_gen_index].1;
            }
            for parent in self.parents_of(oid)? {
                if seen.contains(&parent) {
                    continue;
                }
                let p_gen = commit_rank_for_walk(self, parent);
                if p_gen != UNRANKED_COMMIT && p_gen < min_generation {
                    continue;
                }
                stack.push(parent);
            }
        }
        Ok(merged)
    }
}

/// Dense bitset of ref-tip indices (for shared `--contains` walks).
#[derive(Clone)]
struct TipIndexBitset {
    words: Vec<u64>,
}

impl TipIndexBitset {
    fn with_capacity(tips: usize) -> Self {
        Self {
            words: vec![0; tips.div_ceil(64)],
        }
    }

    fn set(&mut self, index: usize) {
        self.words[index / 64] |= 1_u64 << (index % 64);
    }

    fn is_empty(&self) -> bool {
        self.words.iter().all(|w| *w == 0)
    }

    /// Union `other` into `self`. Returns whether any new bit was set.
    fn or_assign(&mut self, other: &Self) -> bool {
        let mut changed = false;
        for (slot, &bits) in self.words.iter_mut().zip(&other.words) {
            let merged = *slot | bits;
            if merged != *slot {
                *slot = merged;
                changed = true;
            }
        }
        changed
    }

    fn from_index(tip_count: usize, index: usize) -> Self {
        let mut set = Self::with_capacity(tip_count);
        set.set(index);
        set
    }

    fn iter_indices(&self) -> impl Iterator<Item = usize> + '_ {
        self.words.iter().enumerate().flat_map(|(word_idx, word)| {
            let base = word_idx * 64;
            (0..64).filter_map(move |bit| {
                if *word & (1_u64 << bit) != 0 {
                    Some(base + bit)
                } else {
                    None
                }
            })
        })
    }
}

/// Generation/date-ordered queue for [`RefContainsBatch::run`].
///
/// Each commit is popped at most once; tip bitsets merge at enqueue time like [`PaintWalk`].
struct RefContainsWalk {
    tip_count: usize,
    pending: OidHashMap<TipIndexBitset>,
    queued: OidHashSet,
    popped: OidHashSet,
    heap: BinaryHeap<PaintKey>,
    seq: u64,
}

impl RefContainsWalk {
    fn new(tip_count: usize) -> Self {
        Self {
            tip_count,
            pending: oid_hash_map(),
            queued: oid_hash_set(),
            popped: oid_hash_set(),
            heap: BinaryHeap::new(),
            seq: 0,
        }
    }

    fn seed(
        &mut self,
        cache: &mut CommitGraphCache<'_>,
        tip_index: usize,
        oid: ObjectId,
    ) -> Result<()> {
        self.enqueue(
            cache,
            oid,
            TipIndexBitset::from_index(self.tip_count, tip_index),
        )
    }

    fn enqueue(
        &mut self,
        cache: &mut CommitGraphCache<'_>,
        oid: ObjectId,
        bits: TipIndexBitset,
    ) -> Result<()> {
        let entry = self
            .pending
            .entry(oid)
            .or_insert_with(|| TipIndexBitset::with_capacity(self.tip_count));
        if !entry.or_assign(&bits) {
            return Ok(());
        }
        if self.popped.contains(&oid) {
            // A new tip reached this commit after an earlier pop (skewed dates / merge order).
            // Re-queue like [`PaintWalk::mark`] repaint so all active tips walk past here.
            self.popped.remove(&oid);
            self.queued.remove(&oid);
        }
        if self.queued.insert(oid) {
            push_commit_heap_key(cache, &mut self.heap, &mut self.seq, oid)?;
        }
        Ok(())
    }

    fn pop(&mut self) -> Option<(ObjectId, TipIndexBitset)> {
        let node = self.heap.pop()?;
        self.queued.remove(&node.oid);
        self.popped.insert(node.oid);
        let bits = self.pending.remove(&node.oid)?;
        Some((node.oid, bits))
    }
}

/// Per-tip results from one shared backward walk for `--contains` / `--no-contains`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RefContainsTipState {
    /// Any `--contains` target appears in this tip's history (OR semantics).
    pub contains_match: bool,
    /// Any `--no-contains` target appears in this tip's history.
    pub no_contains_match: bool,
}

/// Query-scoped containment for a bounded list: each visited commit remembers whether
/// it reaches any target. Both successful paths and exhausted histories are shared by
/// subsequent tips, without a bitset sized to the entire ref inventory.
pub(crate) struct RefContainsMemo<'repo> {
    graph: CommitGraphCache<'repo>,
    known: OidHashMap<bool>,
    min_target_gen: Option<u64>,
    negative_pending: Vec<ObjectId>,
    negative_frontier: BinaryHeap<PaintKey>,
    negative_seq: u64,
}

struct ContainsFrame {
    oid: ObjectId,
    parents: Vec<ObjectId>,
    next_parent: usize,
}

impl<'repo> RefContainsMemo<'repo> {
    /// Returns `None` for rewritten parents: ancestry need not be acyclic, so
    /// callers must use the batch walk instead of inferring negative ancestors.
    pub(crate) fn new(repo: &'repo Repository, targets: &[ObjectId]) -> Result<Option<Self>> {
        let mut graph = CommitGraphCache::new(repo);
        if graph.has_parent_rewrites()? {
            return Ok(None);
        }
        let min_target_gen = min_ranked_generation(&graph, targets);
        let mut known: OidHashMap<bool> = targets.iter().map(|&oid| (oid, true)).collect();
        let mut negative_pending = Vec::new();
        if graph.graph.is_none() && targets.len() == 1 {
            // Strict ancestors of the sole target cannot contain it. Seed that boundary
            // without reading its ancestors; expensive probes can extend it on demand.
            for parent in graph.parents_of(targets[0])? {
                if parent == targets[0] {
                    return Err(Error::CorruptObject(format!(
                        "commit ancestry cycle at {parent}"
                    )));
                }
                if known.insert(parent, false).is_none() {
                    negative_pending.push(parent);
                }
            }
        }
        Ok(Some(Self {
            graph,
            known,
            min_target_gen,
            negative_pending,
            negative_frontier: BinaryHeap::new(),
            negative_seq: 0,
        }))
    }

    fn known(&self, oid: ObjectId) -> Option<bool> {
        self.known.get(&oid).copied().or_else(|| {
            should_prune_ref_filter_parent(&self.graph, oid, self.min_target_gen).then_some(false)
        })
    }

    fn advance_negative_boundary(&mut self, active: &OidHashMap<usize>) -> Result<Option<usize>> {
        for oid in self.negative_pending.drain(..) {
            push_commit_heap_key(
                &mut self.graph,
                &mut self.negative_frontier,
                &mut self.negative_seq,
                oid,
            )?;
        }
        let mut cut: Option<usize> = None;
        if let Some(key) = self.negative_frontier.pop() {
            for parent in self.graph.parents_of(key.oid)? {
                match self.known.get(&parent) {
                    Some(true) => {
                        return Err(Error::CorruptObject(format!(
                            "commit ancestry cycle at {parent}"
                        )))
                    }
                    Some(false) => {}
                    None => {
                        self.known.insert(parent, false);
                        // Dates choose exploration order only. Every negative result follows
                        // from ancestry of the target, regardless of clock skew.
                        push_commit_heap_key(
                            &mut self.graph,
                            &mut self.negative_frontier,
                            &mut self.negative_seq,
                            parent,
                        )?;
                    }
                }
                if let Some(&index) = active.get(&parent) {
                    cut = Some(cut.map_or(index, |old| old.min(index)));
                }
            }
        }
        Ok(cut)
    }

    /// Evaluate one peeled tip, caching positive and negative ancestry. Committer
    /// dates never exclude ancestry; only valid commit-graph generations may do so.
    pub(crate) fn contains(&mut self, tip: ObjectId) -> Result<bool> {
        if let Some(known) = self.known(tip) {
            return Ok(known);
        }
        let mut active = oid_hash_map();
        active.insert(tip, 0);
        let mut stack = vec![ContainsFrame {
            oid: tip,
            parents: self.graph.parents_of(tip)?,
            next_parent: 0,
        }];
        let mut visited = 1usize;
        while !stack.is_empty() {
            // A nearby positive probe should not walk the target's older history. Once
            // a probe is expensive, advance both histories so an old side branch can
            // meet a proven negative boundary without walking to the repository root.
            if visited >= 1024 {
                if let Some(cut) = self.advance_negative_boundary(&active)? {
                    // Stop the whole active path below a newly proven negative ancestor,
                    // even when the DFS has already descended far beyond that commit.
                    for frame in stack.drain(cut..) {
                        self.known.insert(frame.oid, false);
                        active.remove(&frame.oid);
                    }
                    continue;
                }
            }
            let Some(frame) = stack.last_mut() else {
                break;
            };
            if self.known(frame.oid) == Some(false) {
                active.remove(&frame.oid);
                stack.pop();
                continue;
            }
            let Some(&parent) = frame.parents.get(frame.next_parent) else {
                self.known.insert(frame.oid, false);
                active.remove(&frame.oid);
                stack.pop();
                continue;
            };
            frame.next_parent += 1;
            match self.known(parent) {
                Some(true) => {
                    for frame in stack {
                        self.known.insert(frame.oid, true);
                    }
                    return Ok(true);
                }
                Some(false) => continue,
                None => {}
            }
            if active.insert(parent, stack.len()).is_some() {
                return Err(Error::CorruptObject(format!(
                    "commit ancestry cycle at {parent}"
                )));
            }
            visited += 1;
            stack.push(ContainsFrame {
                oid: parent,
                parents: self.graph.parents_of(parent)?,
                next_parent: 0,
            });
        }
        Ok(false)
    }
}

/// One memoized shared-history walk over all candidate ref tips.
pub(crate) struct RefContainsBatch<'repo> {
    graph: CommitGraphCache<'repo>,
    tips: Vec<ObjectId>,
    tip_state: Vec<RefContainsTipState>,
    tip_index: OidHashMap<usize>,
}

impl<'repo> RefContainsBatch<'repo> {
    /// Start a batch session on `repo`.
    pub(crate) fn new(repo: &'repo Repository) -> Self {
        Self {
            graph: CommitGraphCache::new(repo),
            tips: Vec::new(),
            tip_state: Vec::new(),
            tip_index: oid_hash_map(),
        }
    }

    /// Register a commit tip (already peeled; not re-read); returns its index
    /// (deduplicated by commit oid).
    pub(crate) fn register_tip(&mut self, tip: ObjectId) -> usize {
        if let Some(&idx) = self.tip_index.get(&tip) {
            return idx;
        }
        let idx = self.tips.len();
        self.tips.push(tip);
        self.tip_state.push(RefContainsTipState {
            contains_match: false,
            no_contains_match: false,
        });
        self.tip_index.insert(tip, idx);
        idx
    }

    /// Walk shared history from all registered tips and fill [`Self::tip_state`].
    ///
    /// # Errors
    ///
    /// Propagates object read failures during parent walks.
    pub(crate) fn run(
        &mut self,
        contains_targets: &[ObjectId],
        no_contains_targets: &[ObjectId],
    ) -> Result<()> {
        if self.tips.is_empty() || (contains_targets.is_empty() && no_contains_targets.is_empty()) {
            return Ok(());
        }

        if self.graph.has_parent_rewrites()? {
            self.graph.graph = None;
        }
        self.graph.ensure_single_pack_reader();

        for state in &mut self.tip_state {
            state.contains_match = contains_targets.is_empty();
            state.no_contains_match = false;
        }

        let contains_set: OidHashSet = contains_targets.iter().copied().collect();
        let no_contains_set: OidHashSet = no_contains_targets.iter().copied().collect();

        let filter_targets: Vec<ObjectId> = contains_targets
            .iter()
            .chain(no_contains_targets.iter())
            .copied()
            .collect();
        let min_target_gen = min_ranked_generation(&self.graph, &filter_targets);
        let need_contains = !contains_targets.is_empty();
        let need_no_contains = !no_contains_targets.is_empty();

        let tip_count = self.tips.len();
        let mut walk = RefContainsWalk::new(tip_count);
        for (i, &tip) in self.tips.iter().enumerate() {
            walk.seed(&mut self.graph, i, tip)?;
        }

        let tip_still_active = |state: &RefContainsTipState| -> bool {
            (need_contains && !state.contains_match)
                || (need_no_contains && !state.no_contains_match)
        };

        while let Some((oid, indices_set)) = walk.pop() {
            if indices_set.is_empty() {
                continue;
            }

            if contains_set.contains(&oid) {
                for i in indices_set.iter_indices() {
                    self.tip_state[i].contains_match = true;
                }
            }
            if no_contains_set.contains(&oid) {
                for i in indices_set.iter_indices() {
                    self.tip_state[i].no_contains_match = true;
                }
            }

            let mut active = TipIndexBitset::with_capacity(tip_count);
            for i in indices_set.iter_indices() {
                if tip_still_active(&self.tip_state[i]) {
                    active.set(i);
                }
            }
            if active.is_empty() {
                if self.tip_state.iter().all(|s| !tip_still_active(s)) {
                    break;
                }
                continue;
            }

            for parent in self.graph.parents_of(oid)? {
                if should_prune_ref_filter_parent(&self.graph, parent, min_target_gen) {
                    continue;
                }
                walk.enqueue(&mut self.graph, parent, active.clone())?;
            }
        }
        Ok(())
    }

    /// State for a tip index from [`Self::register_tip`].
    #[must_use]
    pub(crate) fn tip_state(&self, index: usize) -> &RefContainsTipState {
        &self.tip_state[index]
    }
}

fn min_ranked_generation(graph: &CommitGraphCache<'_>, targets: &[ObjectId]) -> Option<u64> {
    let mut min_gen: Option<u64> = None;
    for &target in targets {
        let gen = commit_graph_rank(graph, target)?;
        min_gen = Some(min_gen.map_or(gen, |m| m.min(gen)));
    }
    min_gen
}

fn should_prune_ref_filter_parent(
    graph: &CommitGraphCache<'_>,
    parent: ObjectId,
    min_target_gen: Option<u64>,
) -> bool {
    let Some(min_gen) = min_target_gen else {
        return false;
    };
    let Some(p_gen) = commit_graph_rank(graph, parent) else {
        return false;
    };
    p_gen < min_gen
}

/// Branch tips that are merged into any of `bases` (ancestors of a base commit).
///
/// Both `bases` and `tips` must be commit ids (peel refs and tags first).
///
/// # Errors
///
/// Propagates object read failures during the ancestry walk.
pub(crate) fn branch_tips_merged_into(
    repo: &Repository,
    bases: &[ObjectId],
    tips: &[ObjectId],
) -> Result<OidHashSet> {
    // A tip that is itself a base is merged; when every tip is (listing the branch the
    // base names), nothing needs ranking or walking.
    if tips.iter().all(|tip| bases.contains(tip)) {
        return Ok(tips.iter().copied().collect());
    }
    let mut cache = CommitGraphCache::new(repo);
    cache.tips_merged_from_bases(bases, tips)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::environment::RepositoryOptions;
    use crate::odb::Odb;
    use crate::repo::{InitOptions, Repository};
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn tips_equal_to_bases_are_merged_without_reading_history() -> Result<()> {
        let dir = tempdir()?;
        let repo = open_test_bare(&dir)?;
        let root = write_commit_same_time(repo.odb(), &[], "root", 100)?;
        let tip = write_commit_same_time(repo.odb(), &[root], "tip", 200)?;
        let absent = ObjectId::from_hex("1111111111111111111111111111111111111111")?;
        let merged = branch_tips_merged_into(&repo, &[tip, absent], &[absent])?;
        assert_eq!(merged, [absent].into_iter().collect::<OidHashSet>());
        let merged = branch_tips_merged_into(&repo, &[tip], &[tip, root])?;
        assert_eq!(merged, [tip, root].into_iter().collect::<OidHashSet>());
        let merged = branch_tips_merged_into(&repo, &[root], &[tip, root])?;
        assert_eq!(merged, [root].into_iter().collect::<OidHashSet>());
        Ok(())
    }

    #[test]
    fn containment_memo_shares_successes_and_exhausted_histories() -> Result<()> {
        for with_graph in [false, true] {
            let dir = tempdir()?;
            let repo = open_test_bare(&dir)?;
            let root = write_commit_same_time(repo.odb(), &[], "root", 100)?;
            let target = write_commit_same_time(repo.odb(), &[root], "target", 1000)?;
            let side = write_commit_same_time(repo.odb(), &[root], "side", 200)?;
            let child = write_commit_same_time(repo.odb(), &[target], "skewed", 150)?;
            let merge = write_commit_same_time(repo.odb(), &[side, child], "merge", 160)?;
            let repo = if with_graph {
                with_commit_graph_over_refs(&dir, repo, &[("refs/heads/main", merge)])?
            } else {
                repo
            };
            let mut memo = RefContainsMemo::new(&repo, &[target])?.expect("unrewritten parents");
            assert!(memo.contains(merge)?);
            assert_eq!(memo.known(side), Some(false));
            assert_eq!(memo.known.get(&child), Some(&true));
            assert!(!memo.contains(side)?);
            assert!(memo.contains(child)?);
            assert!(memo.contains(target)?);
        }
        Ok(())
    }

    #[test]
    fn containment_memo_meets_old_side_history_without_reading_the_root() -> Result<()> {
        let dir = tempdir()?;
        let repo = open_test_bare(&dir)?;
        let root = write_commit_same_time(repo.odb(), &[], "root", 100)?;
        let mut trunk = root;
        let mut fork = root;
        for i in 0..5000 {
            trunk = write_commit_same_time(repo.odb(), &[trunk], &format!("trunk {i}"), 1000)?;
            if i == 4100 {
                fork = trunk;
            }
        }
        let target = trunk;
        let mut side = fork;
        for i in 0..2 {
            side = write_commit_same_time(repo.odb(), &[side], &format!("side {i}"), 200)?;
        }
        let mut memo = RefContainsMemo::new(&repo, &[target])?.expect("unrewritten parents");
        assert!(!memo.contains(side)?);
        assert!(
            !memo.known.contains_key(&root),
            "old history must stay unread"
        );
        assert!(!memo.contains(fork)?);
        Ok(())
    }

    #[test]
    fn containment_memo_defers_parent_rewrites_to_batch() -> Result<()> {
        for replace in [false, true] {
            let dir = tempdir()?;
            let repo = open_test_bare(&dir)?;
            let old = write_commit_same_time(repo.odb(), &[], "old", 100)?;
            let target = write_commit_same_time(repo.odb(), &[], "target", 1000)?;
            let replacement = write_commit_same_time(repo.odb(), &[target], "replacement", 1100)?;
            let repo = with_commit_graph_over_refs(
                &dir,
                repo,
                &[("refs/heads/old", old), ("refs/heads/new", replacement)],
            )?;
            if replace {
                crate::refs::write_ref(
                    repo.git_dir(),
                    &format!("refs/replace/{old}"),
                    &replacement,
                )?;
            } else {
                fs::create_dir_all(repo.git_dir().join("info"))?;
                fs::write(
                    repo.git_dir().join("info/grafts"),
                    format!("{old} {target}\n"),
                )?;
            }
            let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
            assert!(RefContainsMemo::new(&repo, &[target])?.is_none());
            let mut batch = RefContainsBatch::new(&repo);
            let index = batch.register_tip(old);
            batch.run(&[target], &[])?;
            assert!(batch.graph.graph.is_none());
            assert!(batch.tip_state(index).contains_match);
        }
        Ok(())
    }

    fn empty_tree(odb: &Odb) -> Result<ObjectId> {
        odb.write_loose_materialize(ObjectKind::Tree, b"")
    }

    fn write_commit(
        odb: &Odb,
        parents: &[ObjectId],
        msg: &str,
        author_time: i64,
        committer_time: i64,
    ) -> Result<ObjectId> {
        let tree = empty_tree(odb)?;
        let mut body = format!("tree {tree}\n");
        for p in parents {
            body.push_str(&format!("parent {p}\n"));
        }
        body.push_str(&format!(
            "author T <t@e.com> {author_time} +0000\ncommitter T <t@e.com> {committer_time} +0000\n\n{msg}\n"
        ));
        odb.write_loose_materialize(ObjectKind::Commit, body.as_bytes())
    }

    fn write_commit_same_time(
        odb: &Odb,
        parents: &[ObjectId],
        msg: &str,
        time: i64,
    ) -> Result<ObjectId> {
        write_commit(odb, parents, msg, time, time)
    }

    fn open_test_bare(dir: &tempfile::TempDir) -> Result<Repository> {
        Repository::init(
            dir.path(),
            &InitOptions {
                bare: true,

                ref_storage: crate::RefStorageFormat::Files,
                ..InitOptions::default()
            },
        )?;
        Repository::open(dir.path(), &RepositoryOptions::empty())
    }

    #[test]
    fn parents_of_reads_through_one_buffer_without_session_copies() -> Result<()> {
        let dir = tempdir()?;
        let repo = open_test_bare(&dir)?;
        let root = write_commit_same_time(repo.odb(), &[], "root", 100)?;
        let mid = write_commit_same_time(repo.odb(), &[root], "mid", 200)?;
        let tip = write_commit_same_time(repo.odb(), &[mid], "tip with a longer message", 300)?;
        let tag = repo.odb().write_loose_materialize(
            ObjectKind::Tag,
            format!("object {tip}\ntype commit\ntag v1\ntagger T <t@e.com> 300 +0000\n\nv1\n")
                .as_bytes(),
        )?;

        let mut cache = CommitGraphCache::with_commit_graph(&repo, false);
        cache.retained = Some(WalkedCommits::default());
        cache.retain_reads = true;
        assert_eq!(cache.parents_of(tip)?, vec![mid]);
        assert_eq!(cache.parents_of(mid)?, vec![root]);
        assert_eq!(cache.parents_of(root)?, Vec::<ObjectId>::new());
        for oid in [tip, mid, root] {
            assert!(
                !repo.odb().session_read_is_cached(&oid),
                "walked commit {oid} must not be copied into the session read cache"
            );
        }
        use crate::porcelain::log::CommitReuse;
        let mut retained = cache.retained.take().expect("retention enabled");
        for (oid, message) in [(tip, "tip with a longer message\n"), (root, "root\n")] {
            let data = retained
                .take_commit(&oid)
                .expect("walked commit is retained")?;
            assert_eq!(data.message, message);
        }

        cache.retain_reads = false;
        assert_eq!(cache.parents_of(tag)?, vec![mid]);
        let _ = repo.odb().read(&mid)?;
        assert!(repo.odb().session_read_is_cached(&mid));
        Ok(())
    }

    #[test]
    fn merge_bases_all_vs_union_matches_a_real_merge_of_the_others() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let root = write_commit_same_time(&odb, &[], "root", 100)?;
        let a1 = write_commit_same_time(&odb, &[root], "a1", 200)?;
        let b1 = write_commit_same_time(&odb, &[root], "b1", 300)?;
        let ours = write_commit_same_time(&odb, &[a1, b1], "ours", 400)?;
        let theirs = write_commit_same_time(&odb, &[b1, a1], "theirs", 500)?;
        let c1 = write_commit_same_time(&odb, &[root], "c1", 600)?;
        let merged = write_commit_same_time(&odb, &[ours, c1], "virtual", 700)?;
        let other = write_commit_same_time(&odb, &[theirs, c1], "other", 800)?;

        assert_eq!(merge_bases_all(&repo, &[ours, theirs])?, vec![b1, a1]);
        assert_eq!(
            merge_bases_all_vs_union(&repo, other, &[ours, c1])?,
            merge_bases_all(&repo, &[other, merged])?,
        );
        assert_eq!(
            merge_bases_all_vs_union(&repo, other, &[ours, c1])?,
            vec![c1, b1, a1]
        );
        assert_eq!(merge_bases_all_vs_union(&repo, a1, &[a1, b1])?, vec![a1]);
        Ok(())
    }

    #[test]
    fn merge_bases_with_equal_dates_keep_discovery_order() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let root = write_commit_same_time(&odb, &[], "root", 100)?;
        let sides: Vec<ObjectId> = (0..4)
            .map(|i| write_commit_same_time(&odb, &[root], &format!("side {i}"), 100))
            .collect::<Result<_>>()?;
        let left = write_commit_same_time(&odb, &sides, "left", 100)?;
        let mut reversed = sides.clone();
        reversed.reverse();
        let right = write_commit_same_time(&odb, &reversed, "right", 100)?;
        // Same dates: the walk visits `left`'s parents in parent order first.
        assert_eq!(merge_bases_all(&repo, &[left, right])?, sides);
        assert_eq!(merge_bases_all(&repo, &[right, left])?, reversed);
        Ok(())
    }

    #[test]
    fn merge_base_unranked_deep_linear_finds_base_without_full_history_scan() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups, reset_intersect_walk_steps,
        };

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let mut parent = write_commit_same_time(&repo.odb(), &[], "root", 100)?;
        for i in 1..3000 {
            parent = write_commit_same_time(&repo.odb(), &[parent], &format!("c{i}"), 100 + i)?;
        }
        let tip = parent;
        let mut old = tip;
        for _ in 0..1500 {
            old = first_parent_oid(&mut CommitGraphCache::new(&repo), old)?;
        }
        {
            let mut link = repo
                .caches()
                .commit_linkage()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            link.parents.clear();
            link.times.clear();
        }
        reset_intersect_walk_steps();
        reset_commit_parent_lookups();
        {
            let mut link = repo
                .caches()
                .commit_linkage()
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            link.parents.clear();
            link.times.clear();
        }
        let bases = merge_bases_first_vs_rest(&repo, tip, &[old])?;
        assert_eq!(bases, vec![old]);
        let parent_reads = commit_parent_lookups();
        assert!(
            parent_reads <= 1600,
            "merge-base should not parse every commit on a 3000-commit chain \
             (parent lookups {parent_reads}, intersect dequeue steps {})",
            super::intersect_walk_test_hooks::intersect_walk_steps()
        );
        assert!(
            parent_reads >= 1400,
            "sanity: should walk from tip toward merge base (parent lookups {parent_reads})"
        );
        Ok(())
    }

    /// Two short branches forking from the middle of a long chain: once only stale commits
    /// are queued the walk stops instead of following the shared tail to the root.
    #[test]
    fn merge_base_of_short_branches_stops_above_the_shared_tail() -> Result<()> {
        use super::intersect_walk_test_hooks::{intersect_walk_steps, reset_intersect_walk_steps};

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 2000, |i| 100 + i as i64)?;
        let fork = chain[1000];
        let mut sides = [fork, fork];
        for (side, tip) in sides.iter_mut().enumerate() {
            for i in 0..3 {
                *tip = write_commit_same_time(
                    &repo.odb(),
                    &[*tip],
                    &format!("side{side}-{i}"),
                    5_000 + i,
                )?;
            }
        }
        for all in [false, true] {
            clear_commit_linkage(&repo);
            reset_intersect_walk_steps();
            let bases = if all {
                merge_bases_all(&repo, &sides)?
            } else {
                merge_bases_first_vs_rest(&repo, sides[0], &sides[1..])?
            };
            assert_eq!(bases, vec![fork], "all={all}");
            let steps = intersect_walk_steps();
            assert!(
                steps <= 16,
                "all={all}: walk dequeued {steps} commits for a 7-commit fork"
            );
        }
        let tip = chain[chain.len() - 1];
        for pair in [[tip, fork], [fork, tip]] {
            reset_intersect_walk_steps();
            assert_eq!(merge_bases_all(&repo, &pair)?, vec![fork]);
            assert_eq!(
                intersect_walk_steps(),
                0,
                "an ancestor tip is the only merge base, found on the first-parent chain"
            );
        }
        Ok(())
    }

    fn clear_commit_linkage(repo: &Repository) {
        let mut link = repo
            .caches()
            .commit_linkage()
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        link.parents.clear();
        link.times.clear();
    }

    /// Linear chain of `len` commits; `time(i)` dates commit `i` (0 is the root).
    fn write_chain(
        repo: &Repository,
        len: usize,
        time: impl Fn(usize) -> i64,
    ) -> Result<Vec<ObjectId>> {
        let mut chain = vec![write_commit_same_time(&repo.odb(), &[], "c0", time(0))?];
        for i in 1..len {
            let parent = chain[i - 1];
            chain.push(write_commit_same_time(
                &repo.odb(),
                &[parent],
                &format!("c{i}"),
                time(i),
            )?);
        }
        Ok(chain)
    }

    /// Classify a pair with a cold cache and return the result with the parent lookups spent.
    fn relation_with_lookups(
        repo: &Repository,
        first: ObjectId,
        second: ObjectId,
    ) -> (FirstParentRelation, usize) {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };
        clear_commit_linkage(repo);
        reset_commit_parent_lookups();
        let relation = first_parent_chain_relation(&mut CommitGraphCache::new(repo), first, second);
        (relation, commit_parent_lookups())
    }

    #[test]
    fn first_parent_chain_relation_stops_near_the_answer() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 3000, |i| 100 + i as i64)?;
        let tip = chain[2999];
        let old = chain[2499];

        let (relation, lookups) = relation_with_lookups(&repo, tip, old);
        assert_eq!(relation, FirstParentRelation::SecondIsAncestor);
        assert!(
            lookups <= 510,
            "descendant first: must not walk the ancestor's 2500-commit chain ({lookups})"
        );

        let (relation, lookups) = relation_with_lookups(&repo, old, tip);
        assert_eq!(relation, FirstParentRelation::FirstIsAncestor);
        assert!(
            lookups <= 510,
            "ancestor first: must not walk the ancestor's 2500-commit chain ({lookups})"
        );

        assert_eq!(
            relation_with_lookups(&repo, tip, tip).0,
            FirstParentRelation::Same
        );
        Ok(())
    }

    #[test]
    fn first_parent_chain_relation_equal_and_skewed_dates_stay_correct() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;

        let flat = write_chain(&repo, 2000, |_| 100)?;
        let (relation, lookups) = relation_with_lookups(&repo, flat[1999], flat[1499]);
        assert_eq!(relation, FirstParentRelation::SecondIsAncestor);
        assert!(
            lookups <= 1010,
            "equal dates step both chains in lockstep ({lookups})"
        );
        let (relation, _) = relation_with_lookups(&repo, flat[1499], flat[1999]);
        assert_eq!(relation, FirstParentRelation::FirstIsAncestor);

        // Every descendant is dated before its parent: the date order is wrong at each step,
        // which may cost extra walking but must not change the answer.
        let skewed = write_chain(&repo, 300, |i| 10_000 - i as i64)?;
        assert_eq!(
            relation_with_lookups(&repo, skewed[299], skewed[100]).0,
            FirstParentRelation::SecondIsAncestor
        );
        assert_eq!(
            relation_with_lookups(&repo, skewed[100], skewed[299]).0,
            FirstParentRelation::FirstIsAncestor
        );
        Ok(())
    }

    #[test]
    fn first_parent_chain_relation_forked_branches_stop_where_chains_meet() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let trunk = write_chain(&repo, 2000, |i| 100 + i as i64)?;
        let fork = trunk[1999];
        let mut tip_a = fork;
        let mut tip_b = fork;
        for i in 0..20 {
            tip_a = write_commit_same_time(&repo.odb(), &[tip_a], &format!("a{i}"), 5_000 + i)?;
            tip_b = write_commit_same_time(&repo.odb(), &[tip_b], &format!("b{i}"), 6_000 + i)?;
        }
        let (relation, lookups) = relation_with_lookups(&repo, tip_a, tip_b);
        assert_eq!(relation, FirstParentRelation::Unrelated);
        assert!(
            lookups <= 60,
            "forked tips must stop at the fork, not scan the 2000-commit trunk ({lookups})"
        );

        // A second-parent ancestor is not on the first-parent chain.
        let merge = write_commit_same_time(&repo.odb(), &[tip_a, tip_b], "merge", 7_000)?;
        assert_eq!(
            relation_with_lookups(&repo, merge, tip_b).0,
            FirstParentRelation::Unrelated
        );
        assert_eq!(
            relation_with_lookups(&repo, merge, tip_a).0,
            FirstParentRelation::SecondIsAncestor
        );
        Ok(())
    }

    #[test]
    fn graph_range_counts_are_bounded_by_distance() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };
        use crate::rev_list::{rev_list, RevListOptions};

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 3000, |i| 100 + i as i64)?;
        std::fs::write(
            repo.git_dir().join("refs/heads/main"),
            format!("{}\n", chain[2999]),
        )?;
        crate::maintenance::write_commit_graph(
            &repo,
            &crate::maintenance::CommitGraphOptions::default(),
        )?;
        let mut lookups = Vec::new();
        for low_level in [false, true] {
            clear_commit_linkage(&repo);
            reset_commit_parent_lookups();
            let count = if low_level {
                rev_list(
                    &repo,
                    &[chain[2999].to_hex()],
                    &[chain[2979].to_hex()],
                    &RevListOptions {
                        count: true,
                        use_commit_graph: true,
                        ..Default::default()
                    },
                )?
                .commit_count()
            } else {
                repo.rev_walk().count(
                    &["main~20..main"],
                    &crate::revwalk::RevWalkListOptions::default(),
                )?
            };
            assert_eq!(count, 20);
            lookups.push(commit_parent_lookups());
        }
        assert!(
            lookups.iter().all(|&n| n <= 60),
            "graph-backed range counts must stay near the range, parent lookups: {lookups:?}"
        );
        Ok(())
    }

    #[test]
    fn count_excluding_ancestors_on_deep_chain_is_bounded_by_distance() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 3000, |i| 100 + i as i64)?;
        let tip = chain[2999];
        let old = chain[2499];

        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let filter = ReachableCountFilter::default();
        assert_eq!(
            count_commits_reachable_excluding_ancestors_of(&repo, &[tip], &[old], filter)?,
            500
        );
        assert_eq!(
            count_commits_reachable_excluding_ancestors_of(&repo, &[old], &[tip], filter)?,
            0
        );
        assert_eq!(merge_bases_first_vs_rest(&repo, old, &[tip])?, vec![old]);
        assert_eq!(merge_bases_first_vs_rest(&repo, tip, &[old])?, vec![old]);
        let lookups = commit_parent_lookups();
        assert!(
            lookups <= 520,
            "left/right counts on a deep chain must not walk to the root ({lookups})"
        );

        let since = ReachableCountFilter {
            since: Some(100 + 2900),
            ..ReachableCountFilter::default()
        };
        assert_eq!(
            count_commits_reachable_excluding_ancestors_of(&repo, &[tip], &[old], since)?,
            100
        );
        Ok(())
    }

    /// Trunk of `len` merges, each bringing in one side commit. Most side commits fork from the
    /// previous trunk commit; every seventh forks 30 merges back, so it sits off the first-parent
    /// chain of the trunk commit it is merged into. Side commit `i` is dated `10 * i`, merge `i`
    /// `10 * i + 5`. Returns the trunk (index 0 is the root).
    fn write_merge_trunk(repo: &Repository, len: usize) -> Result<Vec<ObjectId>> {
        let odb = repo.odb();
        let mut trunk = vec![write_commit_same_time(&odb, &[], "t0", 0)?];
        for i in 1..len {
            let base = if i % 7 == 0 && i > 30 {
                trunk[i - 30]
            } else {
                trunk[i - 1]
            };
            let side = write_commit_same_time(&odb, &[base], &format!("s{i}"), 10 * i as i64)?;
            trunk.push(write_commit_same_time(
                &odb,
                &[trunk[i - 1], side],
                &format!("t{i}"),
                10 * i as i64 + 5,
            )?);
        }
        Ok(trunk)
    }

    #[test]
    fn count_excluding_ancestors_on_merge_heavy_range_is_linear() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, commit_parent_queries, reset_commit_parent_lookups,
            reset_commit_parent_queries,
        };

        const LEN: usize = 1600;
        const OLD: usize = 600;
        const RANGE: usize = 2 * (LEN - 1 - OLD);

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let trunk = write_merge_trunk(&repo, LEN)?;
        let tip = trunk[LEN - 1];
        let old = trunk[OLD];
        let merges_in_range = LEN - 1 - OLD;
        let newest_ten = 10 * (LEN - 10) as i64;
        let oldest_ten = 10 * (OLD + 10) as i64 + 5;
        let cases = [
            (ReachableCountFilter::default(), RANGE),
            (
                ReachableCountFilter {
                    first_parent_only: true,
                    ..ReachableCountFilter::default()
                },
                merges_in_range,
            ),
            (
                ReachableCountFilter {
                    min_parents: Some(2),
                    ..ReachableCountFilter::default()
                },
                merges_in_range,
            ),
            (
                ReachableCountFilter {
                    max_parents: Some(1),
                    ..ReachableCountFilter::default()
                },
                merges_in_range,
            ),
            (
                ReachableCountFilter {
                    since: Some(newest_ten),
                    ..ReachableCountFilter::default()
                },
                20,
            ),
            (
                ReachableCountFilter {
                    until: Some(oldest_ten),
                    ..ReachableCountFilter::default()
                },
                20,
            ),
            (
                ReachableCountFilter {
                    since: Some(newest_ten),
                    first_parent_only: true,
                    ..ReachableCountFilter::default()
                },
                10,
            ),
        ];

        let listed = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[old], None)?;
        assert_eq!(listed.len(), RANGE);

        for with_graph in [false, true] {
            if with_graph {
                std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n"))?;
                crate::maintenance::write_commit_graph(
                    &repo,
                    &crate::maintenance::CommitGraphOptions::default(),
                )?;
            }
            for (filter, expected) in cases {
                clear_commit_linkage(&repo);
                reset_commit_parent_lookups();
                reset_commit_parent_queries();
                assert_eq!(
                    count_commits_reachable_excluding_ancestors_of(&repo, &[tip], &[old], filter)?,
                    expected,
                    "{filter:?}, commit-graph: {with_graph}"
                );
                let (lookups, queries) = (commit_parent_lookups(), commit_parent_queries());
                assert!(
                    lookups <= RANGE + 200 && queries <= 4 * RANGE,
                    "counting must walk the range once, not re-check ancestry per commit \
                     ({filter:?}, commit-graph: {with_graph}, lookups {lookups}, queries {queries})"
                );
                assert_eq!(
                    count_commits_reachable_excluding_ancestors_of(&repo, &[old], &[tip], filter)?,
                    0
                );
            }
        }
        Ok(())
    }

    #[test]
    fn ahead_behind_one_commit_on_deep_chain_is_linear_not_quadratic() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 3000, |i| 100 + i as i64)?;
        let tip = chain[2999];
        let upstream = chain[2998];

        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let (ahead, behind, retained, _) =
            ahead_behind_of_target_commits(&repo, tip, upstream, 1, 0)?;
        assert_eq!((ahead, behind), (1, 0));
        assert_eq!(retained, vec![tip]);
        let lookups = commit_parent_lookups();
        assert!(
            lookups <= 6500,
            "one-commit-ahead status must not re-check ancestry per walked commit ({lookups})"
        );

        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let (ahead2, behind2, _, behind_retained) =
            ahead_behind_of_target_commits(&repo, upstream, tip, 0, 1)?;
        assert_eq!((ahead2, behind2), (0, 1));
        assert_eq!(behind_retained, vec![tip]);
        let lookups = commit_parent_lookups();
        assert!(
            lookups <= 6500,
            "one-commit-behind status must not re-check ancestry per walked commit ({lookups})"
        );
        Ok(())
    }

    #[test]
    fn merge_base_ranked_sibling_tips_do_not_scan_trunk_on_direct_probe() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        const TRUNK_LEN: usize = 2001;
        const BRANCH_LEN: usize = 20;

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let mut parent = write_commit_same_time(&repo.odb(), &[], "root", 100)?;
        for i in 1..TRUNK_LEN {
            parent = write_commit_same_time(
                &repo.odb(),
                &[parent],
                &format!("trunk{i}"),
                100 + i as i64,
            )?;
        }
        let fork = parent;
        let mut tip_a = fork;
        for i in 0..BRANCH_LEN {
            tip_a = write_commit_same_time(
                &repo.odb(),
                &[tip_a],
                &format!("a{i}"),
                100 + TRUNK_LEN as i64 + i as i64,
            )?;
        }
        let mut tip_b = fork;
        for i in 0..BRANCH_LEN {
            tip_b = write_commit_same_time(
                &repo.odb(),
                &[tip_b],
                &format!("b{i}"),
                100 + TRUNK_LEN as i64 + BRANCH_LEN as i64 + i as i64,
            )?;
        }
        use crate::refs::write_ref;
        write_ref(&repo.git_dir(), "refs/heads/branch-a", &tip_a)?;
        write_ref(&repo.git_dir(), "refs/heads/branch-b", &tip_b)?;
        write_ref(&repo.git_dir(), "refs/heads/main", &fork)?;
        repo.odb().write_commit_graph()?;
        assert!(
            dir.path().join("objects/info/commit-graph").is_file(),
            "test fixture must write a commit-graph file"
        );
        let repo = open_test_bare(&dir)?;
        {
            let mut cache = CommitGraphCache::new(&repo);
            assert_ne!(
                commit_rank_for_walk(&mut cache, tip_a),
                UNRANKED_COMMIT,
                "commit-graph must supply generations for ranked merge-base probes"
            );
            assert_ne!(commit_rank_for_walk(&mut cache, tip_b), UNRANKED_COMMIT);
            assert_ne!(commit_rank_for_walk(&mut cache, fork), UNRANKED_COMMIT);
        }

        reset_commit_parent_lookups();
        for _ in 0..10 {
            let bases = merge_bases_first_vs_rest(&repo, tip_a, &[tip_b])?;
            assert_eq!(bases, vec![fork]);
        }
        let parent_reads = commit_parent_lookups();
        assert!(
            parent_reads < 600,
            "ranked direct-ancestor probes must not walk the {TRUNK_LEN}-commit trunk \
             (parent lookups {parent_reads} across 10 calls; full trunk scan would be \
             ~{TRUNK_LEN} per failed probe)"
        );
        Ok(())
    }

    #[test]
    fn commit_rank_for_walk_unranked_without_commit_graph_despite_dates() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let old = write_commit_same_time(&repo.odb(), &[], "old", 946_684_800)?;
        let new = write_commit_same_time(&repo.odb(), &[old], "new", 1_893_456_000)?;
        let mut cache = CommitGraphCache::new(&repo);
        assert_eq!(
            commit_rank_for_walk(&mut cache, new),
            UNRANKED_COMMIT,
            "generation probes must not treat committer dates as ranks without a commit-graph"
        );
        assert_eq!(commit_rank_for_walk(&mut cache, old), UNRANKED_COMMIT);
        Ok(())
    }

    #[test]
    fn merge_base_linear_ancestor_uses_generation_shortcut() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let mut parent = write_commit_same_time(&repo.odb(), &[], "root", 100)?;
        for i in 1..50 {
            parent = write_commit_same_time(&repo.odb(), &[parent], &format!("c{i}"), 100 + i)?;
        }
        let tip = parent;
        let old = {
            let mut p = tip;
            for _ in 0..20 {
                p = first_parent_oid(&mut CommitGraphCache::new(&repo), p)?;
            }
            p
        };
        let bases = merge_bases_first_vs_rest(&repo, tip, &[old])?;
        assert_eq!(bases, vec![old]);
        Ok(())
    }

    #[test]
    fn commits_reachable_excluding_respects_limit() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let c1 = write_commit_same_time(&repo.odb(), &[], "one", 100)?;
        let c2 = write_commit_same_time(&repo.odb(), &[c1], "two", 200)?;
        let c3 = write_commit_same_time(&repo.odb(), &[c2], "three", 300)?;

        let all = commits_reachable_excluding_ancestors_of(&repo, &[c3], &[], None)?;
        assert_eq!(all, vec![c3, c2, c1]);

        let limited = commits_reachable_excluding_ancestors_of(&repo, &[c3], &[], Some(2))?;
        assert_eq!(limited, vec![c3, c2]);

        let (total, retained) = ahead_of_target_commits(&repo, c3, c1, 1)?;
        assert_eq!(total, 2);
        assert_eq!(retained, vec![c3]);

        let ahead = commits_reachable_excluding_ancestors_of(&repo, &[c3], &[c1], None)?;
        assert_eq!(ahead, vec![c3, c2]);

        Ok(())
    }

    #[test]
    fn commits_reachable_excluding_honors_grafts() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let root = write_commit_same_time(&repo.odb(), &[], "root", 100)?;
        let mid = write_commit_same_time(&repo.odb(), &[root], "mid", 200)?;
        let tip = write_commit_same_time(&repo.odb(), &[mid], "tip", 300)?;
        let graft_path = repo.git_dir().join("info/grafts");
        fs::create_dir_all(graft_path.parent().expect("graft parent")).map_err(Error::Io)?;
        fs::write(&graft_path, format!("{tip} {root}\n")).map_err(Error::Io)?;
        let out = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[root], None)?;
        assert_eq!(out, vec![tip]);
        Ok(())
    }

    #[test]
    fn commits_reachable_excluding_stops_at_shallow_boundary() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let missing: ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("oid");
        let tip = write_commit_same_time(&repo.odb(), &[missing], "tip", 100)?;

        fs::write(repo.git_dir().join("shallow"), format!("{tip}\n")).map_err(Error::Io)?;

        let out = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[], None)?;
        assert_eq!(out, vec![tip]);

        Ok(())
    }

    #[test]
    fn count_symmetric_ahead_behind_and_branch_base() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let c1 = write_commit_same_time(&repo.odb(), &[], "one", 100)?;
        let c2 = write_commit_same_time(&repo.odb(), &[c1], "two", 200)?;
        let c3 = write_commit_same_time(&repo.odb(), &[c2], "three", 300)?;
        let (ahead, behind) = count_symmetric_ahead_behind(&repo, c3, c1)?;
        assert_eq!(ahead, 2);
        assert_eq!(behind, 0);
        let (ahead2, behind2, retained, _) = ahead_behind_of_target_commits(&repo, c3, c1, 1, 0)?;
        assert_eq!((ahead2, behind2), (2, 0));
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0], c3);
        let idx = branch_base_for_tip(&repo, c3, &[c1, c2])?;
        assert!(idx.is_some());
        Ok(())
    }

    #[test]
    fn merge_base_for_diff_helpers() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let c1 = write_commit_same_time(&repo.odb(), &[], "one", 100)?;
        let c2 = write_commit_same_time(&repo.odb(), &[c1], "two", 200)?;
        let mb = merge_base_for_diff_two_commits(&repo, c2, c1).expect("mb");
        assert_eq!(mb, c1);
        let mb_idx = merge_base_for_diff_index(&repo, c2, c1).expect("mb idx");
        assert_eq!(mb_idx, c1);
        Ok(())
    }

    #[test]
    fn reachable_walk_since_cutoff_terminates_without_pending() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let c1 = write_commit(&repo.odb(), &[], "one", 100, 100)?;
        let c2 = write_commit(&repo.odb(), &[c1], "two", 200, 200)?;
        let c3 = write_commit(&repo.odb(), &[c2], "three", 300, 300)?;
        let tip = write_commit(&repo.odb(), &[c3], "tip", 400, 400)?;

        let cutoffs = ReachableWalkCommitterCutoffs {
            since: Some(250),
            until: None,
        };
        let mut state = ReachableWalkState::new(&repo, &[tip], &[])?;
        let page1 = state.emit_until(&repo, Some(1), cutoffs)?;
        assert_eq!(page1.len(), 1);
        assert_eq!(page1[0], tip);
        assert!(state.has_pending());

        let page2 = state.emit_until(&repo, Some(10), cutoffs)?;
        assert_eq!(page2, vec![c3]);
        assert!(
            !state.has_pending(),
            "walk must finish once every reachable commit is older than --since"
        );

        let all = state.emit_until(&repo, None, cutoffs)?;
        assert!(all.is_empty());

        Ok(())
    }

    #[test]
    fn commits_reachable_errors_on_missing_parent_without_shallow() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let missing: ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("oid");
        let tip = write_commit_same_time(&repo.odb(), &[missing], "tip", 100)?;

        let err = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[], None).unwrap_err();
        assert!(matches!(err, Error::ObjectNotFound(_)));

        Ok(())
    }

    #[test]
    fn odb_walk_sees_quarantined_commits_that_repo_odb_cannot() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let old = write_commit_same_time(&repo.odb(), &[], "old", 100)?;
        let quarantine =
            crate::receive_quarantine::ReceiveQuarantine::create(&repo.odb(), repo.git_dir())?;
        let q_odb = quarantine.odb();
        let mid = write_commit_same_time(&q_odb, &[old], "mid", 200)?;
        let new = write_commit_same_time(&q_odb, &[mid], "new", 300)?;

        assert!(!repo.odb().exists(&new));
        assert!(is_ancestor_odb_walk(&q_odb, old, new)?);
        assert!(!is_ancestor_odb_walk(&q_odb, new, old)?);
        Ok(())
    }

    #[test]
    fn odb_walk_handles_merges_skew_and_non_commits() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let root = write_commit_same_time(&odb, &[], "root", 1_000)?;
        // Side branch committed with a clock far ahead of its descendants.
        let skewed = write_commit_same_time(&odb, &[root], "skewed", 9_000)?;
        let mut main = root;
        for i in 0..20 {
            main = write_commit_same_time(&odb, &[main], &format!("m{i}"), 2_000 + i)?;
        }
        let merge = write_commit_same_time(&odb, &[main, skewed], "merge", 3_000)?;
        let unrelated = write_commit_same_time(&odb, &[], "unrelated", 2_500)?;

        assert!(is_ancestor_odb_walk(&odb, root, merge)?);
        assert!(is_ancestor_odb_walk(&odb, skewed, merge)?);
        assert!(is_ancestor_odb_walk(&odb, merge, merge)?);
        assert!(!is_ancestor_odb_walk(&odb, merge, root)?);
        assert!(!is_ancestor_odb_walk(&odb, unrelated, merge)?);

        let tree = empty_tree(&odb)?;
        assert!(!is_ancestor_odb_walk(&odb, tree, merge)?);
        assert!(!is_ancestor_odb_walk(&odb, root, tree)?);
        Ok(())
    }

    #[test]
    fn odb_walk_treats_missing_parents_as_history_end() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let missing: ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("oid");
        let base = write_commit_same_time(&odb, &[], "base", 50)?;
        let tip = write_commit_same_time(&odb, &[missing], "tip", 100)?;

        assert!(!is_ancestor_odb_walk(&odb, base, tip)?);
        assert!(matches!(
            is_ancestor_odb_walk(&odb, missing, base),
            Err(Error::ObjectNotFound(_))
        ));
        Ok(())
    }

    /// 2035-01-01 and 2000-01-01, for histories whose dates run backwards.
    const YEAR_2035: i64 = 2_051_222_400;
    const YEAR_2000: i64 = 946_684_800;

    #[test]
    fn odb_walk_follows_descendants_dated_before_the_ancestor() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let root = write_commit_same_time(&odb, &[], "root", YEAR_2000 - 10)?;
        let ancestor = write_commit_same_time(&odb, &[root], "future", YEAR_2035)?;
        let mut tip = ancestor;
        for i in 0..250 {
            tip = write_commit_same_time(&odb, &[tip], &format!("skewed{i}"), YEAR_2000 + i)?;
        }
        let unrelated = write_commit_same_time(&odb, &[], "unrelated", YEAR_2035)?;

        assert!(is_ancestor_odb_walk(&odb, ancestor, tip)?);
        assert!(is_ancestor_odb_walk(&odb, root, tip)?);
        assert!(!is_ancestor_odb_walk(&odb, unrelated, tip)?);
        assert!(!is_ancestor_odb_walk(&odb, tip, ancestor)?);
        Ok(())
    }

    #[test]
    fn odb_walk_matches_exhaustive_reachability_on_random_skewed_histories() -> Result<()> {
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |bound: usize| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            usize::try_from(state % bound as u64).unwrap_or_default()
        };
        for round in 0..4 {
            let dir = tempdir().map_err(Error::Io)?;
            let repo = open_test_bare(&dir)?;
            let odb = repo.odb();
            let quarantine =
                crate::receive_quarantine::ReceiveQuarantine::create(&odb, repo.git_dir())?;
            let q_odb = quarantine.odb();
            let graphed = 40;
            let mut commits: Vec<ObjectId> = Vec::new();
            let mut parents_of: Vec<Vec<usize>> = Vec::new();
            for i in 0..70 {
                let mut parents = Vec::new();
                if i > 0 && next(10) != 0 {
                    parents.push(next(i));
                    if next(3) == 0 {
                        let other = next(i);
                        if !parents.contains(&other) {
                            parents.push(other);
                        }
                    }
                }
                let parent_oids: Vec<ObjectId> = parents.iter().map(|&p| commits[p]).collect();
                let time = 1_000_000 + i64::try_from(next(5_000)).unwrap_or_default();
                let target = if i < graphed { &odb } else { &q_odb };
                commits.push(write_commit_same_time(
                    target,
                    &parent_oids,
                    &format!("r{round}c{i}"),
                    time,
                )?);
                parents_of.push(parents);
                if i == graphed - 1 {
                    for (n, oid) in commits.iter().enumerate() {
                        fs::write(
                            repo.git_dir().join(format!("refs/heads/b{n}")),
                            format!("{oid}\n"),
                        )
                        .map_err(Error::Io)?;
                    }
                    odb.write_commit_graph()?;
                }
            }
            let graph =
                crate::commit_graph_file::CommitGraphChain::load(&repo.git_dir().join("objects"))
                    .expect("commit-graph");

            let reaches = |from: usize, to: usize| {
                let mut stack = vec![from];
                let mut seen = vec![false; parents_of.len()];
                while let Some(n) = stack.pop() {
                    if n == to {
                        return true;
                    }
                    if !std::mem::replace(&mut seen[n], true) {
                        stack.extend(&parents_of[n]);
                    }
                }
                false
            };
            for a in 0..commits.len() {
                for d in 0..commits.len() {
                    let expected = reaches(d, a);
                    assert_eq!(
                        is_ancestor_odb_walk(&q_odb, commits[a], commits[d])?,
                        expected,
                        "round {round}: is {a} an ancestor of {d}"
                    );
                    assert_eq!(
                        is_ancestor_odb_walk_with_graph(
                            &q_odb,
                            Some(&graph),
                            commits[a],
                            commits[d]
                        )?,
                        expected,
                        "round {round} with commit-graph: is {a} an ancestor of {d}"
                    );
                }
            }
        }
        Ok(())
    }

    #[test]
    fn odb_walk_with_graph_bounds_by_generation_not_by_date() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let root = write_commit_same_time(&odb, &[], "root", YEAR_2000 - 10)?;
        let ancestor = write_commit_same_time(&odb, &[root], "future", YEAR_2035)?;
        let mut graphed_tip = ancestor;
        for i in 0..150 {
            graphed_tip =
                write_commit_same_time(&odb, &[graphed_tip], &format!("g{i}"), YEAR_2000 + i)?;
        }
        let unrelated = write_commit_same_time(&odb, &[], "unrelated", YEAR_2035 + 1)?;
        let side = write_commit_same_time(&odb, &[root], "side", YEAR_2035 + 5)?;
        fs::write(
            repo.git_dir().join("refs/heads/main"),
            format!("{graphed_tip}\n"),
        )
        .map_err(Error::Io)?;
        fs::write(
            repo.git_dir().join("refs/heads/u"),
            format!("{unrelated}\n"),
        )
        .map_err(Error::Io)?;
        fs::write(repo.git_dir().join("refs/heads/side"), format!("{side}\n"))
            .map_err(Error::Io)?;
        odb.write_commit_graph()?;
        let graph =
            crate::commit_graph_file::CommitGraphChain::load(&repo.git_dir().join("objects"))
                .expect("commit-graph");
        assert_ne!(graph.commit_generation_for_walk(&ancestor), UNRANKED_COMMIT);
        assert_ne!(
            graph.commit_generation_for_walk(&graphed_tip),
            UNRANKED_COMMIT
        );

        // Pushed commits outside the graph, dated before the ancestor.
        let quarantine =
            crate::receive_quarantine::ReceiveQuarantine::create(&odb, repo.git_dir())?;
        let q_odb = quarantine.odb();
        let mut pushed_tip = graphed_tip;
        for i in 0..120 {
            pushed_tip =
                write_commit_same_time(&q_odb, &[pushed_tip], &format!("p{i}"), YEAR_2000 + i)?;
        }

        let walk = |a, d| is_ancestor_odb_walk_with_graph(&q_odb, Some(&graph), a, d);
        assert!(walk(ancestor, graphed_tip)?);
        assert!(walk(ancestor, pushed_tip)?);
        assert!(walk(root, pushed_tip)?);
        assert!(walk(graphed_tip, pushed_tip)?);
        assert!(!walk(unrelated, pushed_tip)?);
        assert!(!walk(side, pushed_tip)?);
        assert!(!walk(pushed_tip, ancestor)?);
        for (a, d) in [
            (ancestor, pushed_tip),
            (unrelated, pushed_tip),
            (side, graphed_tip),
        ] {
            assert_eq!(walk(a, d)?, is_ancestor_odb_walk(&q_odb, a, d)?);
        }
        Ok(())
    }

    #[test]
    fn left_behind_walk_ignores_hidden_ref_forked_near_root() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let trunk = write_chain(&repo, 2000, |i| 1_000 + i as i64)?;
        let old_side = write_commit_same_time(&repo.odb(), &[trunk[1]], "old side", 1_002)?;
        let stray = write_commit_same_time(&repo.odb(), &[trunk[1999]], "stray", 5_000)?;
        let hide = [trunk[1999], old_side];
        let repo = with_commit_graph_over_refs(
            &dir,
            repo,
            &[
                ("refs/heads/main", trunk[1999]),
                ("refs/tags/old", old_side),
            ],
        )?;

        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let exhaustive = walk_commits_reachable_excluding_ancestors_of(
            &repo,
            &[stray],
            &hide,
            ReachableWalkLimit::CountAllRetainNewest(20),
        )?;
        let exhaustive_lookups = commit_parent_lookups();
        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let bounded = count_commits_left_behind(&repo, &[stray], &hide, 20)?;
        let lookups = commit_parent_lookups();

        assert_eq!(bounded, (1, vec![stray]));
        assert_eq!(bounded, exhaustive);
        assert!(
            exhaustive_lookups >= 1900,
            "the converging paint walks the trunk down to the old fork ({exhaustive_lookups})"
        );
        assert!(
            lookups <= 50,
            "an old hidden side branch must not walk the trunk to its fork ({lookups})"
        );
        Ok(())
    }

    /// Point `refs` at their commits, write a reachable commit-graph and reopen the repository so
    /// walks load it.
    fn with_commit_graph_over_refs(
        dir: &tempfile::TempDir,
        repo: Repository,
        refs: &[(&str, ObjectId)],
    ) -> Result<Repository> {
        for (name, oid) in refs {
            crate::refs::write_ref(repo.git_dir(), name, oid)?;
        }
        repo.write_commit_graph(&crate::maintenance::CommitGraphOptions::default())?;
        drop(repo);
        Repository::open(dir.path(), &RepositoryOptions::empty())
    }

    /// A ref reaches `skewed`, a commit only the old tip seems to reach, through a child dated
    /// before it; ten newer commits on another ref come first in date order.
    struct SkewedHistory {
        old_tip: ObjectId,
        skewed: ObjectId,
        hide: Vec<ObjectId>,
        refs: Vec<(&'static str, ObjectId)>,
    }

    fn write_skewed_history(repo: &Repository) -> Result<SkewedHistory> {
        let odb = repo.odb();
        let base = write_commit_same_time(&odb, &[], "base", 100)?;
        let skewed = write_commit_same_time(&odb, &[base], "dated ahead", 1_000)?;
        let old_tip = write_commit_same_time(&odb, &[skewed], "old tip", 1_100)?;
        let behind = write_commit_same_time(&odb, &[skewed], "dated behind", 150)?;
        let held = write_commit_same_time(&odb, &[behind], "held", 200)?;
        let mut filler = base;
        for i in 0..10 {
            filler = write_commit_same_time(&odb, &[filler], &format!("filler {i}"), 500 + i)?;
        }
        Ok(SkewedHistory {
            old_tip,
            skewed,
            hide: vec![base, held, filler],
            refs: vec![
                ("refs/heads/main", base),
                ("refs/heads/held", held),
                ("refs/heads/filler", filler),
            ],
        })
    }

    #[test]
    fn left_behind_walk_is_exact_under_clock_skew_without_commit_graph() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_skewed_history(&repo)?;

        let bounded = count_commits_left_behind(&repo, &[history.old_tip], &history.hide, 20)?;

        assert_eq!(
            bounded,
            (1, vec![history.old_tip]),
            "{} is reachable from refs/heads/held",
            history.skewed
        );
        Ok(())
    }

    #[test]
    fn left_behind_walk_is_exact_under_clock_skew_with_commit_graph() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_skewed_history(&repo)?;
        let repo = with_commit_graph_over_refs(&dir, repo, &history.refs)?;

        let bounded = count_commits_left_behind(&repo, &[history.old_tip], &history.hide, 20)?;

        assert_eq!(bounded, (1, vec![history.old_tip]));
        Ok(())
    }

    #[test]
    fn left_behind_walk_hides_commits_a_descendant_ref_reaches() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        let base = write_commit_same_time(&odb, &[], "base", 100)?;
        let shared = write_commit_same_time(&odb, &[base], "shared", 200)?;
        let kept = write_commit_same_time(&odb, &[shared], "kept by tag", 300)?;
        let lost_a = write_commit_same_time(&odb, &[shared], "lost a", 400)?;
        let lost_b = write_commit_same_time(&odb, &[lost_a], "lost b", 500)?;
        let target = write_commit_same_time(&odb, &[base], "target", 600)?;
        let hide = [target, kept];

        let bounded = count_commits_left_behind(&repo, &[lost_b], &hide, 20)?;
        let exhaustive = walk_commits_reachable_excluding_ancestors_of(
            &repo,
            &[lost_b],
            &hide,
            ReachableWalkLimit::CountAllRetainNewest(20),
        )?;

        assert_eq!(bounded, (2, vec![lost_b, lost_a]));
        assert_eq!(bounded, exhaustive);
        let capped = count_commits_left_behind(&repo, &[lost_b], &hide, 1)?;
        assert_eq!(capped, (2, vec![lost_b]));
        Ok(())
    }

    #[test]
    fn left_behind_walk_without_commit_graph_stops_near_the_tips() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        const HOUR: i64 = 3_600;
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let trunk = write_chain(&repo, 2000, |i| 1_000_000 + HOUR * i as i64)?;
        let old_tag = trunk[3];
        let wip =
            write_commit_same_time(&repo.odb(), &[trunk[1999]], "wip", 1_000_000 + HOUR * 2000)?;
        let hide = [trunk[1999], old_tag];

        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let exhaustive = walk_commits_reachable_excluding_ancestors_of(
            &repo,
            &[wip],
            &hide,
            ReachableWalkLimit::CountAllRetainNewest(20),
        )?;
        let exhaustive_lookups = commit_parent_lookups();
        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        let bounded = count_commits_left_behind(&repo, &[wip], &hide, 20)?;
        let lookups = commit_parent_lookups();

        assert_eq!(bounded, (1, vec![wip]));
        assert_eq!(bounded, exhaustive);
        assert!(
            exhaustive_lookups >= 1900,
            "the converging paint walks down to the old tag ({exhaustive_lookups})"
        );
        // One day of hourly commits behind the tips, then the slop.
        assert!(
            lookups <= 24 + PAINT_SLOP + 10,
            "without a commit-graph an old tag must not walk history ({lookups})"
        );
        Ok(())
    }

    #[test]
    fn left_behind_walk_without_commit_graph_repaints_within_the_slop() -> Result<()> {
        const DAY: i64 = 86_400;
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let odb = repo.odb();
        // `shared` is dated days after the hide chain that reaches it, so the walk pops it as
        // left-only before that chain arrives, beyond the skew allowance.
        let shared = write_commit_same_time(&odb, &[], "shared", 100 * DAY)?;
        let old_tip = write_commit_same_time(&odb, &[shared], "old tip", 101 * DAY)?;
        let mut held = shared;
        for i in 0..3 {
            held = write_commit_same_time(&odb, &[held], &format!("held {i}"), 10 * DAY + i)?;
        }

        let bounded = count_commits_left_behind(&repo, &[old_tip], &[held], 20)?;

        assert_eq!(bounded, (1, vec![old_tip]));
        Ok(())
    }

    #[test]
    fn left_behind_walk_with_nothing_hidden_counts_all_history() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 30, |i| 100 + i as i64)?;

        let (total, newest) = count_commits_left_behind(&repo, &[chain[29]], &[], 3)?;

        assert_eq!(total, 30);
        assert_eq!(newest, vec![chain[29], chain[28], chain[27]]);
        Ok(())
    }

    /// Point `refs` at their commits, pack everything they reach with whole objects (no
    /// deltas), and reopen the repository so reads go through the pack.
    fn packed_over_refs(
        dir: &tempfile::TempDir,
        repo: Repository,
        refs: &[(String, ObjectId)],
    ) -> Result<Repository> {
        for (name, oid) in refs {
            crate::refs::write_ref(repo.git_dir(), name, oid)?;
        }
        let opts = crate::maintenance::RepackOptions {
            pack_build: Some(crate::pack_objects::PackBuildOptions {
                delta: false,
                window: 0,
                max_depth: 0,
                ..crate::pack_objects::PackBuildOptions::for_local_copy(None)
            }),
            ..crate::maintenance::RepackOptions::default()
        };
        crate::maintenance::repack(&repo, &opts)?;
        drop(repo);
        Repository::open(dir.path(), &RepositoryOptions::empty())
    }

    /// A trunk with one old tagged side commit per tag, every tag older than the trunk tip.
    struct ManyTags {
        trunk: Vec<ObjectId>,
        tags: Vec<ObjectId>,
        refs: Vec<(String, ObjectId)>,
    }

    fn write_many_tags(repo: &Repository, count: usize) -> Result<ManyTags> {
        const HOUR: i64 = 3_600;
        let trunk = write_chain(repo, count + 10, |i| 1_000_000 + HOUR * i as i64)?;
        let mut tags = Vec::with_capacity(count);
        let mut refs = vec![("refs/heads/main".to_owned(), trunk[count + 9])];
        for i in 0..count {
            let tag = write_commit_same_time(
                &repo.odb(),
                &[trunk[i]],
                &format!("release {i}\n\nA longer message body for release {i}."),
                1_000_000 + HOUR * i as i64 + 60,
            )?;
            refs.push((format!("refs/tags/v{i}"), tag));
            tags.push(tag);
        }
        Ok(ManyTags { trunk, tags, refs })
    }

    #[test]
    fn prime_tips_records_the_linkage_a_full_read_records() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_many_tags(&repo, PRIME_TIPS_MIN + 16)?;
        let repo = packed_over_refs(&dir, repo, &history.refs)?;

        let mut expected = Vec::new();
        clear_commit_linkage(&repo);
        let mut full = CommitGraphCache::new(&repo);
        for &tag in &history.tags {
            expected.push((full.parents_of(tag)?, full.commit_time(tag)?));
        }

        clear_commit_linkage(&repo);
        let mut primed = CommitGraphCache::new(&repo);
        primed.prime_tips(&history.tags);
        assert_eq!(primed.header_primed.len(), history.tags.len());
        for (&tag, want) in history.tags.iter().zip(&expected) {
            assert_eq!(primed.peel_tip(tag)?, Some(tag));
            let recorded = primed.with_linkage(|link| {
                (
                    link.parents.get(&tag).cloned(),
                    link.times.get(&tag).copied(),
                )
            });
            assert_eq!(recorded, (Some(want.0.clone()), Some(want.1)), "{tag}");
        }

        clear_commit_linkage(&repo);
        let mut few = CommitGraphCache::new(&repo);
        few.prime_tips(&history.tags[..PRIME_TIPS_MIN - 1]);
        assert!(
            few.header_primed.is_empty(),
            "a paint with few tips keeps the plain reads"
        );
        Ok(())
    }

    #[test]
    fn prime_tips_leaves_loose_commits_and_tags_to_the_full_peel() -> Result<()> {
        use crate::objects::{serialize_tag, TagData};

        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_many_tags(&repo, PRIME_TIPS_MIN)?;
        let repo = packed_over_refs(&dir, repo, &history.refs)?;
        let loose = write_commit_same_time(&repo.odb(), &[history.tags[0]], "loose", 9_999_999)?;
        let tag_bytes = serialize_tag(&TagData {
            object: history.tags[1],
            object_type: "commit".into(),
            tag: "annotated".into(),
            tagger: Some("T <t@e.com> 1 +0000".into()),
            message: "annotated\n".into(),
            extra_headers: Vec::new(),
        });
        let annotated = repo.odb().write(ObjectKind::Tag, &tag_bytes)?;
        let mut tips = history.tags.clone();
        tips.extend([loose, annotated]);

        clear_commit_linkage(&repo);
        let mut cache = CommitGraphCache::new(&repo);
        cache.prime_tips(&tips);
        assert_eq!(cache.header_primed.len(), history.tags.len());
        assert!(!cache.header_primed.contains(&loose));
        assert!(!cache.header_primed.contains(&annotated));
        assert_eq!(cache.peel_tip(loose)?, Some(loose));
        assert_eq!(cache.peel_tip(annotated)?, Some(history.tags[1]));
        Ok(())
    }

    #[test]
    fn left_behind_walk_over_many_packed_tags_matches_the_exhaustive_walk() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_many_tags(&repo, 2 * PRIME_TIPS_MIN)?;
        let tip = *history.trunk.last().expect("trunk");
        let repo = packed_over_refs(&dir, repo, &history.refs)?;
        let wip = write_commit_same_time(&repo.odb(), &[tip], "wip", 9_000_000)?;
        let mut hide: Vec<ObjectId> = history.refs.iter().map(|(_, oid)| *oid).collect();

        for expected in [(1, vec![wip]), (0, Vec::new())] {
            clear_commit_linkage(&repo);
            let exhaustive = walk_commits_reachable_excluding_ancestors_of(
                &repo,
                &[wip],
                &hide,
                ReachableWalkLimit::CountAllRetainNewest(20),
            )?;
            clear_commit_linkage(&repo);
            let bounded = count_commits_left_behind(&repo, &[wip], &hide, 20)?;
            assert_eq!(bounded, expected);
            assert_eq!(bounded, exhaustive);
            // A ref on a descendant of `wip` keeps it reachable.
            let child = write_commit_same_time(&repo.odb(), &[wip], "child", 9_000_100)?;
            hide.push(child);
        }
        Ok(())
    }

    #[test]
    fn left_behind_walk_after_a_recording_reads_none_of_the_recorded_tips() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };
        const DAY: i64 = 86_400;
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_many_tags(&repo, 2 * PRIME_TIPS_MIN)?;
        let tip = *history.trunk.last().expect("trunk");
        let repo = packed_over_refs(&dir, repo, &history.refs)?;
        let recorded_at = 5_000_000;
        let now = recorded_at + 30 * DAY;
        let hide: Vec<ObjectId> = history.refs.iter().map(|(_, oid)| *oid).collect();
        let wip = write_commit_same_time(&repo.odb(), &[tip], "wip", recorded_at + 2 * DAY)?;
        let next = write_commit_same_time(&repo.odb(), &[wip], "next", recorded_at + 3 * DAY)?;

        for tips in [vec![wip], vec![next]] {
            clear_commit_linkage(&repo);
            let full = count_commits_left_behind(&repo, &tips, &hide, 20)?;
            clear_commit_linkage(&repo);
            reset_commit_parent_lookups();
            let after =
                count_commits_left_behind_after(&repo, &tips, &[tip], 20, recorded_at, now)?;
            assert_eq!(after, Some(full), "{tips:?}");
            assert!(
                commit_parent_lookups() < 16,
                "the recorded tags must not be read: {} lookups",
                commit_parent_lookups()
            );
        }
        assert_eq!(
            count_commits_left_behind_after(&repo, &[next], &[tip], 1, recorded_at, now)?,
            Some((2, vec![next]))
        );
        Ok(())
    }

    #[test]
    fn left_behind_walk_after_a_recording_gives_up_outside_its_dates() -> Result<()> {
        const DAY: i64 = 86_400;
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let history = write_many_tags(&repo, 4)?;
        let tip = *history.trunk.last().expect("trunk");
        let repo = packed_over_refs(&dir, repo, &history.refs)?;
        let recorded_at = 5_000_000;
        let now = recorded_at + 30 * DAY;
        let left = |time: i64| -> Result<Option<(usize, Vec<ObjectId>)>> {
            let wip = write_commit_same_time(&repo.odb(), &[tip], "wip", time)?;
            clear_commit_linkage(&repo);
            count_commits_left_behind_after(&repo, &[wip], &[tip], 20, recorded_at, now)
        };
        // Within the skew allowance of the recording, or more than it past `now`.
        assert_eq!(left(recorded_at + DAY / 2)?, None);
        assert_eq!(left(recorded_at + DAY)?, None);
        assert_eq!(left(now + 2 * DAY)?, None);
        assert!(left(recorded_at + DAY + 1)?.is_some());
        assert!(left(now + DAY)?.is_some());

        // An older commit further down the left-behind history stops the walk too.
        let old = write_commit_same_time(&repo.odb(), &[tip], "old", recorded_at - DAY)?;
        let fresh = write_commit_same_time(&repo.odb(), &[old], "fresh", recorded_at + 2 * DAY)?;
        clear_commit_linkage(&repo);
        assert_eq!(
            count_commits_left_behind_after(&repo, &[fresh], &[tip], 20, recorded_at, now)?,
            None
        );

        // Grafts rewrite parents, so a recorded ref could reach anything.
        std::fs::create_dir_all(repo.git_dir().join("info"))?;
        std::fs::write(
            repo.git_dir().join("info/grafts"),
            format!("{} {}\n", history.trunk[1], history.trunk[0]),
        )?;
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;
        let wip = write_commit_same_time(&repo.odb(), &[tip], "grafted", recorded_at + 2 * DAY)?;
        assert_eq!(
            count_commits_left_behind_after(&repo, &[wip], &[tip], 20, recorded_at, now)?,
            None
        );
        Ok(())
    }

    #[test]
    fn rewound_ref_is_not_an_ancestor_without_the_slop_walk() -> Result<()> {
        use super::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };

        const HOUR: i64 = 3_600;
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let trunk = write_chain(&repo, 400, |i| 1_000_000 + HOUR * i as i64)?;
        let wip = write_commit_same_time(&repo.odb(), &[trunk[399]], "wip", 9_000_000)?;
        let side = write_commit_same_time(&repo.odb(), &[trunk[200]], "side", 9_000_000)?;

        for (ancestor, descendant) in [
            (wip, trunk[399]),
            (wip, trunk[396]),
            (trunk[396], wip),
            (side, trunk[399]),
            (trunk[100], side),
            (trunk[399], trunk[399]),
        ] {
            clear_commit_linkage(&repo);
            let expected = is_ancestor(&repo, ancestor, descendant)?;
            clear_commit_linkage(&repo);
            assert_eq!(
                is_ancestor_unless_rewound(&repo, ancestor, descendant)?,
                expected,
                "{ancestor} -> {descendant}"
            );
        }

        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        assert!(is_ancestor(&repo, wip, trunk[396]).is_ok_and(|a| !a));
        let slop_lookups = commit_parent_lookups();
        clear_commit_linkage(&repo);
        reset_commit_parent_lookups();
        assert!(!is_ancestor_unless_rewound(&repo, wip, trunk[396])?);
        let lookups = commit_parent_lookups();
        assert!(slop_lookups >= 100, "the date-slop walk ({slop_lookups})");
        // The target, then `wip` and the three commits down the chain to it.
        assert!(lookups <= 5, "HEAD~3 is found on the chain ({lookups})");
        Ok(())
    }

    #[test]
    fn rewound_check_defers_to_the_full_walk_when_grafts_rewrite_parents() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let base = write_commit_same_time(&repo.odb(), &[], "base", 100)?;
        let tip = write_commit_same_time(&repo.odb(), &[base], "tip", 200)?;
        // The graft makes `base` a child of `tip`: each reaches the other.
        fs::create_dir_all(repo.git_dir().join("info"))?;
        fs::write(
            repo.git_dir().join("info/grafts"),
            format!("{base} {tip}\n"),
        )?;
        let repo = Repository::open(dir.path(), &RepositoryOptions::empty())?;

        clear_commit_linkage(&repo);
        let expected = is_ancestor(&repo, tip, base)?;
        clear_commit_linkage(&repo);
        assert_eq!(is_ancestor_unless_rewound(&repo, tip, base)?, expected);
        assert!(expected, "the graft makes {tip} an ancestor of {base}");
        Ok(())
    }
}
