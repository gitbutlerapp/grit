//! Merge-base and reachability primitives.
//!
//! This module implements the subset needed by `grit merge-base`:
//! default merge-base selection, `--all`, `--octopus`, `--independent`,
//! and `--is-ancestor`.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashSet, VecDeque};

use crate::error::{Error, Result};
use crate::ewah_bitmap::Bitmap;
use crate::objects::{
    oid_hash_map, oid_hash_set, oid_hash_set_from, parse_commit, CommitRef, ObjectId, ObjectKind,
    OidHashMap, OidHashSet,
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
pub fn merge_bases_all(repo: &Repository, commits: &[ObjectId]) -> Result<Vec<ObjectId>> {
    if commits.is_empty() {
        return Ok(Vec::new());
    }
    if commits.len() == 1 {
        return Ok(vec![commits[0]]);
    }
    let mut cache = CommitGraphCache::new(repo);
    if commits.len() == 2 {
        let walked = walk_intersecting_histories(&mut cache, commits[0], &[commits[1]], true)?;
        let candidates: OidHashSet = walked.into_iter().collect();
        let mut bases = reduce_to_best(candidates, &mut cache)?;
        sort_merge_bases_for_git_output(&mut cache, &mut bases)?;
        return Ok(bases);
    }
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
    let mut heap = BinaryHeap::new();
    for (oid, color, (time, parents)) in [
        (ancestor, FROM_ANCESTOR, ancestor_linkage),
        (descendant, FROM_DESCENDANT, descendant_linkage),
    ] {
        paint.insert(oid, color);
        parents_of.insert(oid, parents);
        times.insert(oid, time);
        heap.push((time, oid));
    }
    descendant_only.insert(descendant);

    while !descendant_only.is_empty() {
        let Some((_, oid)) = heap.pop() else {
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
            heap.push((time, parent));
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
}

/// When [`SidePaint::compute_hiding`] may stop painting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum HidePaintStop {
    /// Paint until every queued commit is reachable from both sides. Exact under clock skew,
    /// but a hide tip that forked near the root keeps the walk going down to that fork.
    #[default]
    Converged,
    /// Stop once no queued commit is reachable from the tips alone, after a few more commits
    /// of slop, like Git's limited revision walk. Exact with commit-graph generations; heavily
    /// skewed committer dates without them can misreport, as in Git.
    TipsResolved,
}

/// Commits [`HidePaintStop::TipsResolved`] keeps painting after the tips are resolved.
const HIDE_PAINT_SLOP: usize = 5;

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
        };
        for &tip in tips {
            if state.hide.contains(&tip) {
                continue;
            }
            if state.queued.insert(tip) {
                state.push_on_heap(&mut cache, tip, true)?;
            }
        }
        Ok(state)
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
        let time = cache.commit_time(oid)?;
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
                self.after_processed(cache, oid)?;
                let stop = self.heap.is_empty()
                    || self
                        .heap
                        .iter()
                        .all(|pending| pending.time < cutoffs.since.unwrap_or(i64::MIN));
                if stop {
                    self.heap.clear();
                    return Ok(None);
                }
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
        let mut out = Vec::new();
        while let Some(oid) = self.pop_next_emit(&mut cache, cutoffs)? {
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
/// [`ReachableWalkLimit::CountAllRetainNewest`], except that the hide paint stops as soon as
/// `tips` are resolved ([`HidePaintStop::TipsResolved`]). A hidden ref that forked from `tips`
/// near the root then costs nothing, which matters because callers hide every ref in the
/// repository.
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
    state.hide_paint_stop = HidePaintStop::TipsResolved;
    let mut cache = CommitGraphCache::new(repo);
    let mut newest = Vec::new();
    let mut total = 0usize;
    while let Some(oid) =
        state.pop_next_emit(&mut cache, ReachableWalkCommitterCutoffs::default())?
    {
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
/// Traverses through commits filtered out by parent count (Git `rev-list` parent
/// filters still walk the full graph). Uses the commit-graph when loaded and no
/// parent or date filters are active.
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

    let single_relation = if tips.len() == 1 && hide.len() == 1 {
        let relation = first_parent_chain_relation(&mut cache, tips[0], hide[0]);
        if matches!(
            relation,
            FirstParentRelation::Same | FirstParentRelation::FirstIsAncestor
        ) {
            return Ok(0);
        }
        Some(relation)
    } else {
        None
    };

    if filter.min_parents.is_none()
        && filter.max_parents.is_none()
        && filter.since.is_none()
        && filter.until.is_none()
    {
        if let Some(graph) = &cache.graph {
            let shallow: HashSet<ObjectId> = cache.shallow_boundaries.iter().copied().collect();
            if let Some(n) =
                graph.count_reachable_commits(tips, hide, filter.first_parent_only, &shallow)
            {
                return Ok(n);
            }
        }
    }

    // Walking a merge-free chain down to `hide` costs one commit per counted commit, so it
    // runs before the bitmap probe, which pays for opening and checking the bitmap file.
    if single_relation == Some(FirstParentRelation::SecondIsAncestor) {
        if let Some(n) =
            try_count_on_first_parent_chain_excluding_hide(&mut cache, tips[0], hide[0], filter)?
        {
            return Ok(n);
        }
    }

    if tips.len() == 1
        && hide.len() == 1
        && filter.min_parents.is_none()
        && filter.max_parents.is_none()
        && filter.since.is_none()
        && filter.until.is_none()
        && !filter.first_parent_only
    {
        if let Some(n) = try_count_diff_via_stored_commit_bitmaps(repo, tips[0], hide[0])? {
            return Ok(n);
        }
    }

    let mut stack: Vec<ObjectId> = tips.to_vec();
    let mut walked = oid_hash_set();
    let mut count = 0usize;
    while let Some(oid) = stack.pop() {
        if !walked.insert(oid) {
            continue;
        }
        if commit_is_excluded_from_count(&mut cache, hide, oid)? {
            // Ancestors of `hide` cannot reach counted commits; do not walk past them.
            continue;
        }
        let parents = cache.parents_of(oid)?;
        let parent_count = parents.len();
        let time = cache.commit_time(oid)?;
        let in_range = filter.since.is_none_or(|since| time >= since)
            && filter.until.is_none_or(|until| time <= until);
        let parent_ok = parent_count >= filter.min_parents.unwrap_or(0)
            && parent_count <= filter.max_parents.unwrap_or(usize::MAX);
        if in_range && parent_ok {
            count += 1;
        }
        let parent_iter = if filter.first_parent_only {
            parents.into_iter().take(1).collect::<Vec<_>>()
        } else {
            parents
        };
        for parent in parent_iter {
            stack.push(parent);
        }
    }
    Ok(count)
}

/// Like [`first_parent_commits_with_cutoffs`] but only when every visited commit has at most
/// one parent (linear history). Returns `None` when a merge is encountered so callers can
/// fall back to a full-graph bounded walk.
pub fn try_linear_chain_commits_with_cutoffs(
    repo: &Repository,
    tip: ObjectId,
    limit: usize,
    cutoffs: ReachableWalkCommitterCutoffs,
) -> Result<Option<Vec<ObjectId>>> {
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
            stack.extend(parents);
            continue;
        }
        if let Some(&parent) = parents.first() {
            stack.push(parent);
        }
    }
    Ok(Some(out))
}

/// Newest-first commits on the first-parent chain from `tip`, applying committer-date
/// cutoffs, stopping after `limit` matches (or when the chain ends).
pub fn first_parent_commits_with_cutoffs(
    repo: &Repository,
    tip: ObjectId,
    limit: usize,
    cutoffs: ReachableWalkCommitterCutoffs,
) -> Result<Vec<ObjectId>> {
    let mut cache = CommitGraphCache::new(repo);
    let mut out = Vec::with_capacity(limit.min(64));
    let mut stack = vec![tip];
    while let Some(cur) = stack.pop() {
        if out.len() >= limit {
            break;
        }
        let time = cache.commit_time(cur)?;
        let below_since = cutoffs.since.is_some_and(|since| time < since);
        let above_until = cutoffs.until.is_some_and(|until| time > until);
        if !below_since && !above_until {
            out.push(cur);
        }
        let parents = cache.parents_of(cur)?;
        if below_since {
            stack.extend(parents);
            continue;
        }
        if let Some(&parent) = parents.first() {
            stack.push(parent);
        }
    }
    Ok(out)
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

fn commit_is_excluded_from_count(
    cache: &mut CommitGraphCache<'_>,
    hide: &[ObjectId],
    oid: ObjectId,
) -> Result<bool> {
    if hide.len() == 1 {
        let hide_tip = hide[0];
        match first_parent_chain_relation(cache, oid, hide_tip) {
            FirstParentRelation::Same | FirstParentRelation::FirstIsAncestor => return Ok(true),
            FirstParentRelation::SecondIsAncestor => return Ok(false),
            FirstParentRelation::Unrelated => {}
        }
        if cache.is_ancestor(oid, hide_tip)? {
            return Ok(true);
        }
        if cache.is_ancestor(hide_tip, oid)? {
            return Ok(false);
        }
        return Ok(false);
    }
    if hide.contains(&oid) {
        return Ok(true);
    }
    for &hide_tip in hide {
        if cache.is_ancestor(oid, hide_tip)? {
            return Ok(true);
        }
    }
    Ok(false)
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
    let object = repo.odb().read(&oid)?;
    if object.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "object {oid} is not a commit"
        )));
    }
    let commit = parse_commit(&object.data)?;
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
}

#[derive(Eq, PartialEq)]
struct RankedCommit {
    rank: u64,
    committed_at: i64,
    oid: ObjectId,
}

impl Ord for RankedCommit {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        match self.rank.cmp(&other.rank) {
            std::cmp::Ordering::Equal => self.committed_at.cmp(&other.committed_at),
            ord => ord,
        }
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
    ) -> Result<Self> {
        let walk = PaintWalk {
            skip_missing_right: true,
            open_while_left_only: stop == HidePaintStop::TipsResolved,
            slop: match stop {
                HidePaintStop::Converged => 0,
                HidePaintStop::TipsResolved => HIDE_PAINT_SLOP,
            },
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
        for (tips, side) in [(left, PAINT_LEFT), (right, PAINT_RIGHT)] {
            for &tip in tips {
                let Some(tip) = try_peel_to_commit_for_merge_base(cache.repo, tip)? else {
                    continue;
                };
                walk.mark(cache, tip, side)?;
            }
        }
        let mut slop = walk.slop;
        loop {
            if walk.open > 0 {
                slop = walk.slop;
            } else if slop == 0 {
                break;
            } else {
                slop -= 1;
            }
            let Some(node) = walk.heap.pop() else {
                break;
            };
            if walk.queued.remove(&node.oid) == Some(true) {
                walk.open -= 1;
            }
            walk.popped.insert(node.oid);
            let side = walk.sides.get(&node.oid).copied().unwrap_or(0);
            for parent in cache.parents_of(node.oid)? {
                walk.mark(cache, parent, side)?;
            }
        }
        Ok(Self { sides: walk.sides })
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
        Self { sides }
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
    /// Only commits reachable from the left side alone keep the walk going, instead of every
    /// commit not yet reachable from both sides ([`HidePaintStop::TipsResolved`]).
    open_while_left_only: bool,
    /// Commits to keep popping once nothing keeps the walk going.
    slop: usize,
}

impl PaintWalk {
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
        let unresolved = if self.open_while_left_only {
            after == PAINT_LEFT
        } else {
            after != PAINT_BOTH
        };
        let is_open = unresolved || repaint;
        self.queued.insert(oid, is_open);
        if is_open {
            self.open += 1;
        }
        let rank = commit_rank_for_walk(cache, oid);
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
    pending.push(RankedCommit {
        rank,
        committed_at,
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
            if pending.is_empty() {
                tally.apply_label(before, -1);
                return Some(node.oid);
            }
            // Queue holds only stale commits: no new merge-base candidates can appear.
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
}

impl<'r> CommitGraphCache<'r> {
    fn new(repo: &'r Repository) -> Self {
        Self::with_commit_graph(repo, true)
    }

    /// `use_commit_graph: false` leaves [`Self::graph`] empty so every lookup parses the commit.
    fn with_commit_graph(repo: &'r Repository, use_commit_graph: bool) -> Self {
        let cfg = repo.config().map(|c| (*c).clone()).unwrap_or_default();
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
            repo.caches()
                .commit_graph_chain(&repo.git_dir().join("objects"))
                .map(|arc| (*arc).clone())
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
        }
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

    fn ensure_single_pack_reader(&mut self) {
        if self.single_pack.is_some() {
            return;
        }
        let objects_dir = self.repo.git_dir().join("objects");
        if let Ok(Some(reader)) = crate::pack::SingleLocalPackReader::open(&objects_dir) {
            self.single_pack = Some(reader);
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
        let commit_oid = peel_to_commit_for_merge_base(self.repo, oid).map_err(|e| match e {
            Error::InvalidRef(msg) => Error::CorruptObject(msg),
            other => other,
        })?;
        self.ensure_single_pack_reader();
        let (time, parsed_parents) = if let Some(reader) = self.single_pack.as_mut() {
            let object = reader.read_object(&commit_oid)?;
            if object.kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            let linkage = CommitRef::parse(&object.data)?;
            (linkage.committer_timestamp(), linkage.parents().to_vec())
        } else {
            let object = self.repo.odb().read(&commit_oid)?;
            if object.kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            let linkage = CommitRef::parse(&object.data)?;
            (linkage.committer_timestamp(), linkage.parents().to_vec())
        };
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
        Ok(parents)
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
    fn tips_merged_from_bases(
        &mut self,
        bases: &[ObjectId],
        tips: &[ObjectId],
    ) -> Result<OidHashSet> {
        let mut merged = oid_hash_set();
        if bases.is_empty() || tips.is_empty() {
            return Ok(merged);
        }

        let mut tip_gens: Vec<(ObjectId, u64)> = Vec::with_capacity(tips.len());
        for &tip in tips {
            let peeled = peel_to_commit_for_merge_base(self.repo, tip).unwrap_or(tip);
            tip_gens.push((peeled, commit_rank_for_walk(self, peeled)));
        }
        tip_gens.sort_by_key(|(_, gen)| *gen);
        let mut min_gen_index = 0usize;
        let mut min_generation = tip_gens[0].1;

        let tip_set: OidHashSet = tip_gens.iter().map(|(tip, _)| *tip).collect();

        let mut stack: Vec<ObjectId> = Vec::new();
        for &base in bases {
            let peeled = peel_to_commit_for_merge_base(self.repo, base).unwrap_or(base);
            stack.push(peeled);
        }

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

    /// Register a peeled commit tip; returns its index (deduplicated by commit oid).
    pub(crate) fn register_tip(&mut self, repo: &'repo Repository, tip: ObjectId) -> usize {
        let peeled = peel_to_commit_for_merge_base(repo, tip).unwrap_or(tip);
        if let Some(&idx) = self.tip_index.get(&peeled) {
            return idx;
        }
        let idx = self.tips.len();
        self.tips.push(peeled);
        self.tip_state.push(RefContainsTipState {
            contains_match: false,
            no_contains_match: false,
        });
        self.tip_index.insert(peeled, idx);
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

    /// Peeled commit oid for a tip index from [`Self::register_tip`].
    #[must_use]
    pub(crate) fn tip_commit(&self, index: usize) -> ObjectId {
        self.tips[index]
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
/// # Errors
///
/// Propagates object read failures during the ancestry walk.
pub(crate) fn branch_tips_merged_into(
    repo: &Repository,
    bases: &[ObjectId],
    tips: &[ObjectId],
) -> Result<OidHashSet> {
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
    fn left_behind_walk_with_nothing_hidden_counts_all_history() -> Result<()> {
        let dir = tempdir().map_err(Error::Io)?;
        let repo = open_test_bare(&dir)?;
        let chain = write_chain(&repo, 30, |i| 100 + i as i64)?;

        let (total, newest) = count_commits_left_behind(&repo, &[chain[29]], &[], 3)?;

        assert_eq!(total, 30);
        assert_eq!(newest, vec![chain[29], chain[28], chain[27]]);
        Ok(())
    }
}
