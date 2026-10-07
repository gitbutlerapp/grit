//! Merge-base and reachability primitives.
//!
//! This module implements the subset needed by `grit merge-base`:
//! default merge-base selection, `--all`, `--octopus`, `--independent`,
//! and `--is-ancestor`.

use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

use crate::error::{Error, Result};
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::promisor::{read_promisor_missing_oids, repo_treats_promisor_packs};
use crate::reflog::read_reflog;
use crate::repo::Repository;
use crate::rev_parse::{
    peel_to_commit_for_merge_base, resolve_revision, resolve_upstream_symbolic_name,
    upstream_suffix_info,
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
    let mut cache = CommitGraphCache::new(repo);
    let first_anc = cache.ancestor_closure(first)?;
    let mut others_union = HashSet::new();
    for &other in others {
        others_union.extend(cache.ancestor_closure(other)?);
    }
    let candidates: HashSet<ObjectId> = first_anc.intersection(&others_union).copied().collect();
    reduce_to_best(candidates, &mut cache)
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
    reduce_to_best(common, &mut cache)
}

/// All merge bases common to every supplied commit (intersection of ancestor sets,
/// reduced to minimal bases). Matches `git merge-base` with two or more tips.
///
/// This is the same intersection-and-reduction as [`merge_bases_octopus`]; the name
/// documents the `git merge-base A B C ...` calling convention.
pub fn merge_bases_all(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    merge_bases_octopus(repo, commits)
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

    let logs_dir = repo.git_dir.join("logs");
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

    let mut best = HashSet::new();
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

    let entries = read_reflog(&repo.git_dir, &reflog_ref)
        .map_err(|e| Error::Message(format!("failed to read reflog for '{reflog_ref}': {e}")))?;

    let mut candidates = Vec::new();
    let mut seen = HashSet::new();

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
    cache.ancestor_closure(tip)
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
    let left = ancestor_closure(repo, local)?;
    let right = ancestor_closure(repo, other)?;
    let ahead = left.difference(&right).count();
    let behind = right.difference(&left).count();
    Ok((ahead, behind))
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
    if tips.is_empty() {
        return Ok((0, Vec::new()));
    }

    let mut cache = CommitGraphCache::new(repo);
    let mut heap: BinaryHeap<(i64, u64, ObjectId)> = BinaryHeap::new();
    let mut queued = HashSet::new();
    let mut seq = 0u64;
    for &tip in tips {
        if hide.contains(&tip) {
            continue;
        }
        if queued.insert(tip) {
            let time = cache.commit_time(tip)?;
            heap.push((time, seq, tip));
            seq += 1;
        }
    }

    let mut out = Vec::new();
    let mut emitted = 0usize;
    let mut done = HashSet::new();

    let (stop_early_at, retain_newest) = match limit {
        ReachableWalkLimit::StopAfter(n) => (Some(n), None),
        ReachableWalkLimit::CountAllRetainNewest(n) => (None, Some(n)),
        ReachableWalkLimit::Unlimited => (None, None),
    };

    while let Some((_time, _seq, oid)) = heap.pop() {
        if !done.insert(oid) {
            continue;
        }

        let mut hidden = false;
        for &hide_tip in hide {
            if oid == hide_tip || cache.is_ancestor(oid, hide_tip)? {
                hidden = true;
                break;
            }
        }
        if hidden {
            continue;
        }

        emitted += 1;
        if let Some(k) = retain_newest {
            if out.len() < k {
                out.push(oid);
            }
        } else {
            out.push(oid);
            if stop_early_at.is_some_and(|n| out.len() >= n) {
                break;
            }
        }

        for parent in cache.parents_of(oid)? {
            if queued.insert(parent) {
                let time = cache.commit_time(parent)?;
                heap.push((time, seq, parent));
                seq += 1;
            }
        }
    }

    Ok((emitted, out))
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

pub fn commits_reachable_excluding_ancestors_of(
    repo: &Repository,
    tips: &[ObjectId],
    hide: &[ObjectId],
    limit: Option<usize>,
) -> Result<Vec<ObjectId>> {
    let walk_limit = match limit {
        Some(n) => ReachableWalkLimit::StopAfter(n),
        None => ReachableWalkLimit::Unlimited,
    };
    let (_emitted, oids) =
        walk_commits_reachable_excluding_ancestors_of(repo, tips, hide, walk_limit)?;
    Ok(oids)
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
    let tip_positions: HashMap<ObjectId, usize> = tip_chain
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
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    let commit = parse_commit(&object.data)?;
    Ok(commit.parents.first().copied())
}

fn ensure_is_commit(repo: &Repository, oid: ObjectId) -> Result<()> {
    let object = repo.odb.read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    Ok(())
}

fn reduce_to_best(
    candidates: HashSet<ObjectId>,
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
            if cache.ancestor_closure(other)?.contains(&candidate) {
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

struct CommitGraphCache<'r> {
    repo: &'r Repository,
    parents: HashMap<ObjectId, Vec<ObjectId>>,
    closures: HashMap<ObjectId, HashSet<ObjectId>>,
    promisor_stop: std::collections::HashSet<ObjectId>,
    shallow_boundaries: HashSet<ObjectId>,
    /// Committer timestamp (unix seconds) per visited oid, recorded alongside
    /// parents so the date-pruned [`Self::is_ancestor`] walk needs no extra reads.
    times: HashMap<ObjectId, i64>,
    /// Commit-graph file, when present: supplies parents + committer time without
    /// decompressing commit objects. Commits absent from it (or octopus merges)
    /// fall back to [`Self::parents_of`]'s object read.
    graph: Option<crate::commit_graph_file::CommitGraphChain>,
}

impl<'r> CommitGraphCache<'r> {
    fn new(repo: &'r Repository) -> Self {
        let cfg = repo.config().map(|c| (*c).clone()).unwrap_or_default();
        // Stop ancestry traversal only at genuinely-missing promisor objects, not
        // at every member of a promisor pack. The clone base commit lives in a
        // promisor pack but is fully present locally; treating it as a stop point
        // truncates ancestry and breaks fast-forward detection on a push from a
        // partial clone (t5616 "after fetching descendants of non-promisor
        // commits, gc works"). Missing parents are already handled by
        // `parents_of` returning no parents on `ObjectNotFound`, so we only need
        // to record the OIDs the partial clone knows are absent.
        let promisor_stop = if repo_treats_promisor_packs(&repo.git_dir, &cfg) {
            read_promisor_missing_oids(&repo.git_dir)
                .into_iter()
                .collect::<HashSet<ObjectId>>()
        } else {
            HashSet::new()
        };
        let graph = crate::commit_graph_file::CommitGraphChain::load(&repo.git_dir.join("objects"));
        let shallow_boundaries = load_shallow_boundaries(&repo.git_dir);
        Self {
            repo,
            parents: HashMap::new(),
            closures: HashMap::new(),
            promisor_stop,
            shallow_boundaries,
            times: HashMap::new(),
            graph,
        }
    }

    fn ancestor_closure(&mut self, start: ObjectId) -> Result<HashSet<ObjectId>> {
        if let Some(existing) = self.closures.get(&start) {
            return Ok(existing.clone());
        }

        let mut visited = HashSet::new();
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
        if let Some(parents) = self.parents.get(&oid) {
            return Ok(parents.clone());
        }
        if self.promisor_stop.contains(&oid) {
            self.parents.insert(oid, Vec::new());
            self.times.insert(oid, 0);
            return Ok(Vec::new());
        }
        if self.shallow_boundaries.contains(&oid) {
            let commit_oid =
                peel_to_commit_for_merge_base(self.repo, oid).map_err(|e| match e {
                    Error::InvalidRef(msg) => Error::CorruptObject(msg),
                    other => other,
                })?;
            let object = self.repo.odb.read(&commit_oid)?;
            if object.kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            let commit = parse_commit(&object.data)?;
            self.times.insert(
                oid,
                crate::ident::committer_timestamp_for_until_filter(&commit.committer),
            );
            self.parents.insert(oid, Vec::new());
            return Ok(Vec::new());
        }
        // Fast path: parents + committer time from the commit-graph file (no
        // object decompression). Misses (not in graph / octopus) fall through.
        if let Some(graph) = &self.graph {
            if let Some((parents, ctime)) = graph.graph_commit(&oid) {
                self.times.insert(oid, ctime);
                let parents = if self.shallow_boundaries.contains(&oid) {
                    Vec::new()
                } else {
                    parents
                        .into_iter()
                        .filter(|p| !self.promisor_stop.contains(p))
                        .collect()
                };
                self.parents.insert(oid, parents.clone());
                return Ok(parents);
            }
        }
        let commit_oid = peel_to_commit_for_merge_base(self.repo, oid).map_err(|e| match e {
            Error::InvalidRef(msg) => Error::CorruptObject(msg),
            other => other,
        })?;
        let object = self.repo.odb.read(&commit_oid)?;
        if object.kind != ObjectKind::Commit {
            return Err(Error::CorruptObject(format!(
                "object {commit_oid} is not a commit"
            )));
        }
        let commit = parse_commit(&object.data)?;
        self.times.insert(
            oid,
            crate::ident::committer_timestamp_for_until_filter(&commit.committer),
        );
        let parents: Vec<ObjectId> = if self.shallow_boundaries.contains(&commit_oid) {
            Vec::new()
        } else {
            commit
                .parents
                .iter()
                .copied()
                .filter(|p| !self.promisor_stop.contains(p))
                .collect()
        };
        self.parents.insert(oid, parents.clone());
        Ok(parents)
    }

    /// Committer timestamp (unix seconds) for `oid`, peeling tags to their commit.
    /// Populated as a side effect of [`Self::parents_of`]; missing/unreadable
    /// objects report `0` (treated as oldest, so they prune away).
    fn commit_time(&mut self, oid: ObjectId) -> Result<i64> {
        if let Some(t) = self.times.get(&oid) {
            return Ok(*t);
        }
        self.parents_of(oid)?;
        Ok(self.times.get(&oid).copied().unwrap_or(0))
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
        let mut visited: HashSet<ObjectId> = HashSet::new();
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::objects::ObjectKind;
    use crate::odb::Odb;
    use crate::repo::init_bare_clone_minimal;
    use std::fs;
    use tempfile::tempdir;

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
        init_bare_clone_minimal(dir.path(), "main", "files")?;
        Repository::open(dir.path(), None)
    }

    #[test]
    fn commits_reachable_excluding_respects_limit() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let c1 = write_commit_same_time(&repo.odb, &[], "one", 100)?;
        let c2 = write_commit_same_time(&repo.odb, &[c1], "two", 200)?;
        let c3 = write_commit_same_time(&repo.odb, &[c2], "three", 300)?;

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
    fn commits_reachable_excluding_stops_at_shallow_boundary() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let missing: ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("oid");
        let tip = write_commit_same_time(&repo.odb, &[missing], "tip", 100)?;

        fs::write(repo.git_dir.join("shallow"), format!("{tip}\n")).map_err(Error::Io)?;

        let out = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[], None)?;
        assert_eq!(out, vec![tip]);

        Ok(())
    }

    #[test]
    fn commits_reachable_errors_on_missing_parent_without_shallow() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let missing: ObjectId = "0000000000000000000000000000000000000001"
            .parse()
            .expect("oid");
        let tip = write_commit_same_time(&repo.odb, &[missing], "tip", 100)?;

        let err = commits_reachable_excluding_ancestors_of(&repo, &[tip], &[], None).unwrap_err();
        assert!(matches!(err, Error::ObjectNotFound(_)));

        Ok(())
    }
}
