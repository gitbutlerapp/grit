//! Merge-base and reachability primitives.
//!
//! This module implements the subset needed by `grit merge-base`:
//! default merge-base selection, `--all`, `--octopus`, `--independent`,
//! and `--is-ancestor`.

use std::cmp::Ordering;
use std::collections::{BTreeSet, BinaryHeap, HashMap, HashSet, VecDeque};

use crate::error::{Error, Result};
use crate::ewah_bitmap::Bitmap;
use crate::objects::{parse_commit, ObjectId, ObjectKind};
use crate::pack_bitmap::BitmapIndex;
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
        if let Some(base) = try_direct_ancestor_merge_base(&mut cache, other, first)? {
            return Ok(vec![base]);
        }
        if let Some(base) = try_direct_ancestor_merge_base(&mut cache, first, other)? {
            return Ok(vec![base]);
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
    let mut others_union = HashSet::new();
    for &other in others {
        others_union.extend(cache.ancestor_closure(other)?);
    }
    let candidates: HashSet<ObjectId> = first_anc.intersection(&others_union).copied().collect();
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
        let candidates: HashSet<ObjectId> = walked.into_iter().collect();
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

/// Reachability test using only the object database (no commit-graph setup).
///
/// Suitable for client-side fast-forward checks on modest histories (e.g. push).
pub fn is_ancestor_odb_walk(
    odb: &crate::odb::Odb,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<bool> {
    if ancestor == descendant {
        return Ok(true);
    }
    let mut seen = HashSet::new();
    let mut queue = VecDeque::from([descendant]);
    while let Some(cur) = queue.pop_front() {
        if !seen.insert(cur) {
            continue;
        }
        if cur == ancestor {
            return Ok(true);
        }
        let obj = odb.read(&cur)?;
        if obj.kind != ObjectKind::Commit {
            continue;
        }
        let commit = parse_commit(&obj.data)?;
        for parent in commit.parents {
            queue.push_back(parent);
        }
    }
    Ok(false)
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

    let entries = read_reflog(repo.git_dir(), &reflog_ref)
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
    let (ahead, behind, _) = ahead_behind_of_target_commits(repo, local, other, 0)?;
    Ok((ahead, behind))
}

/// Symmetric ahead/behind between `head` and `target`, plus bounded newest-ahead OIDs.
///
/// Matches `git rev-list --left-right --count HEAD...TARGET` for the counts. The returned
/// OID list contains up to `retain_newest_ahead` commits reachable from `head` but not
/// from `target` (newest first), without materializing full ancestor closures.
///
/// # Errors
///
/// Propagates object read and parse errors from commit traversal.
pub fn ahead_behind_of_target_commits(
    repo: &Repository,
    head: ObjectId,
    target: ObjectId,
    retain_newest_ahead: usize,
) -> Result<(usize, usize, Vec<ObjectId>)> {
    if head == target {
        return Ok((0, 0, Vec::new()));
    }
    let target_is_ancestor = is_ancestor(repo, target, head)?;
    let head_is_ancestor = is_ancestor(repo, head, target)?;

    let (ahead, ahead_oids) = if head_is_ancestor && !target_is_ancestor {
        (0, Vec::new())
    } else {
        walk_commits_reachable_excluding_ancestors_of(
            repo,
            std::slice::from_ref(&head),
            &[target],
            ReachableWalkLimit::CountAllRetainNewest(retain_newest_ahead),
        )?
    };

    let behind = if target_is_ancestor && !head_is_ancestor {
        0
    } else {
        walk_commits_reachable_excluding_ancestors_of(
            repo,
            std::slice::from_ref(&target),
            &[head],
            ReachableWalkLimit::CountAllRetainNewest(0),
        )?
        .0
    };
    Ok((ahead, behind, ahead_oids))
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
/// Ordering matches Git's default `rev-list`: among commits ready to print, take the
/// greatest committer timestamp; a parent becomes ready only after all of its children
/// still in the walk have been processed.
#[derive(Clone, Debug)]
pub struct ReachableWalkState {
    heap: BinaryHeap<WalkDateKey>,
    done: HashSet<ObjectId>,
    queued: HashSet<ObjectId>,
    /// Selected children not yet emitted, per parent (Git `limit_list` gating).
    unfinished_children: HashMap<ObjectId, usize>,
    /// Tips seeded on the heap up front (may emit before all children are shown).
    seeded: HashSet<ObjectId>,
    seq: u64,
    hide: Vec<ObjectId>,
    /// Commits excluded because they are a hide tip or an ancestor of one.
    hide_set: HashSet<ObjectId>,
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
    /// Pending child counts per parent OID.
    pub unfinished_children: Vec<(ObjectId, u32)>,
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
        let hide_vec: Vec<ObjectId> = hide.to_vec();
        let mut cache = CommitGraphCache::new(repo);
        let mut state = Self {
            heap: BinaryHeap::new(),
            done: HashSet::new(),
            queued: HashSet::new(),
            unfinished_children: HashMap::new(),
            seeded: HashSet::new(),
            seq: 0,
            hide: hide_vec,
            hide_set: HashSet::new(),
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
            unfinished_children,
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
            unfinished_children: unfinished_children
                .into_iter()
                .map(|(oid, count)| (oid, count as usize))
                .collect(),
            seeded: seeded.into_iter().collect(),
            seq,
            hide_set: HashSet::new(),
            hide,
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
            unfinished_children: self
                .unfinished_children
                .iter()
                .map(|(oid, count)| (*oid, *count as u32))
                .collect(),
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

    fn is_hidden(&mut self, cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> Result<bool> {
        if self.hide.contains(&oid) || self.hide_set.contains(&oid) {
            return Ok(true);
        }
        for &hide_tip in &self.hide {
            if cache.is_ancestor(oid, hide_tip)? {
                self.hide_set.insert(oid);
                return Ok(true);
            }
        }
        Ok(false)
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
        for parent in cache.parents_of(oid)? {
            *self.unfinished_children.entry(parent).or_insert(0) += 1;
        }
        let time = cache.commit_time(oid)?;
        let seq = self.seq;
        self.seq += 1;
        self.heap.push(WalkDateKey { oid, time, seq });
        Ok(())
    }

    fn after_processed(&mut self, cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> Result<()> {
        for parent in cache.parents_of(oid)? {
            let Some(count) = self.unfinished_children.get_mut(&parent) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0 && !self.seeded.contains(&parent) && self.queued.insert(parent) {
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
        let mut cache = CommitGraphCache::new(repo);
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

    if tips.len() == 1 && hide.len() == 1 {
        let tip = tips[0];
        let hide_tip = hide[0];
        if tip != hide_tip
            && try_direct_first_parent_chain_merge_base(&mut cache, tip, hide_tip)?.is_some()
        {
            return Ok(0);
        }
    }

    if filter.min_parents.is_none()
        && filter.max_parents.is_none()
        && filter.since.is_none()
        && filter.until.is_none()
    {
        if let Some(graph) = &cache.graph {
            if let Some(n) = graph.count_reachable_commits(
                tips,
                hide,
                filter.first_parent_only,
                &cache.shallow_boundaries,
            ) {
                return Ok(n);
            }
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

    if tips.len() == 1 && hide.len() == 1 {
        if let Some(n) =
            try_count_on_first_parent_chain_excluding_hide(&mut cache, tips[0], hide[0], filter)?
        {
            return Ok(n);
        }
    }

    let mut stack: Vec<ObjectId> = tips.to_vec();
    let mut walked = HashSet::new();
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

fn try_count_on_first_parent_chain_excluding_hide(
    cache: &mut CommitGraphCache<'_>,
    tip: ObjectId,
    hide: ObjectId,
    filter: ReachableCountFilter,
) -> Result<Option<usize>> {
    if tip == hide {
        return Ok(Some(0));
    }
    if try_direct_first_parent_chain_merge_base(cache, tip, hide)?.is_some() {
        return Ok(Some(0));
    }
    if try_direct_first_parent_chain_merge_base(cache, hide, tip)?.is_none() {
        return Ok(None);
    }

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
        if oid == hide_tip {
            return Ok(true);
        }
        if try_direct_first_parent_chain_merge_base(cache, hide_tip, oid)?.is_some() {
            return Ok(false);
        }
        if try_direct_first_parent_chain_merge_base(cache, oid, hide_tip)?.is_some() {
            return Ok(true);
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
mod intersect_walk_test_hooks {
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

/// When `ancestor` is a direct ancestor of `descendant` on first-parent chains, return it
/// using commit-graph generations to bound the walk.
fn try_direct_ancestor_merge_base(
    cache: &mut CommitGraphCache<'_>,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<Option<ObjectId>> {
    if ancestor == descendant {
        return Ok(Some(ancestor));
    }
    let desc_gen = commit_rank_for_walk(cache, descendant);
    let anc_gen = commit_rank_for_walk(cache, ancestor);
    if desc_gen != UNRANKED_COMMIT && anc_gen != UNRANKED_COMMIT {
        if desc_gen <= anc_gen {
            // Ranked commits: cannot be a strict first-parent ancestor; avoid scanning to root.
            return Ok(None);
        }
        let mut cur = descendant;
        let mut gen = desc_gen;
        while gen > anc_gen {
            cur = first_parent_oid(cache, cur)?;
            gen = commit_rank_for_walk(cache, cur);
            if gen == UNRANKED_COMMIT {
                return Ok(None);
            }
        }
        return Ok((cur == ancestor).then_some(ancestor));
    }
    try_direct_first_parent_chain_merge_base(cache, ancestor, descendant)
}

/// When commit-graph generations are unavailable, test whether `ancestor` lies on
/// `descendant`'s first-parent chain (common for linear history without a graph file).
fn try_direct_first_parent_chain_merge_base(
    cache: &mut CommitGraphCache<'_>,
    ancestor: ObjectId,
    descendant: ObjectId,
) -> Result<Option<ObjectId>> {
    let mut cur = descendant;
    loop {
        if cur == ancestor {
            return Ok(Some(ancestor));
        }
        cur = match first_parent_oid(cache, cur) {
            Ok(parent) => parent,
            Err(_) => return Ok(None),
        };
    }
}

fn commit_rank_for_walk(cache: &mut CommitGraphCache<'_>, oid: ObjectId) -> u64 {
    if let Some(graph) = &cache.graph {
        let rank = graph.commit_generation_for_walk(&oid);
        if rank != UNRANKED_COMMIT {
            return rank;
        }
    }
    UNRANKED_COMMIT
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
    labels: &mut HashMap<ObjectId, u8>,
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
    labels: &mut HashMap<ObjectId, u8>,
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
    let mut labels: HashMap<ObjectId, u8> = HashMap::new();
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
        let candidates: HashSet<ObjectId> = bases.into_iter().collect();
        bases = reduce_to_best(candidates, cache)?;
    }
    sort_merge_bases_for_git_output(cache, &mut bases)?;
    Ok(bases)
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
    closures: HashMap<ObjectId, HashSet<ObjectId>>,
    promisor_stop: std::collections::HashSet<ObjectId>,
    shallow_boundaries: HashSet<ObjectId>,
    /// Commit-graph file, when present: supplies parents + committer time without
    /// decompressing commit objects. Commits absent from it (or octopus merges)
    /// fall back to [`Self::parents_of`]'s object read.
    graph: Option<crate::commit_graph_file::CommitGraphChain>,
    /// Monotonic pack cursor for single-pack repos (deep-history benchmarks).
    single_pack: Option<crate::pack::SingleLocalPackReader>,
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
        let promisor_stop = if repo_treats_promisor_packs(repo.git_dir(), &cfg) {
            read_promisor_missing_oids(repo.git_dir())
                .into_iter()
                .collect::<HashSet<ObjectId>>()
        } else {
            HashSet::new()
        };
        let graph = repo
            .caches()
            .commit_graph_chain(&repo.git_dir().join("objects"))
            .map(|arc| (*arc).clone());
        let shallow_boundaries = load_shallow_boundaries(repo.git_dir());
        Self {
            repo,
            closures: HashMap::new(),
            promisor_stop,
            shallow_boundaries,
            graph,
            single_pack: None,
        }
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
        if let Some(parents) = self.with_linkage(|link| link.parents.get(&oid).cloned()) {
            return Ok(parents);
        }
        #[cfg(test)]
        intersect_walk_test_hooks::record_commit_parent_lookup();
        if self.promisor_stop.contains(&oid) {
            self.with_linkage(|link| {
                link.parents.insert(oid, Vec::new());
                link.times.insert(oid, 0);
            });
            return Ok(Vec::new());
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
            let commit = parse_commit(&object.data)?;
            let time = crate::ident::committer_timestamp_for_until_filter(&commit.committer);
            self.with_linkage(|link| {
                link.times.insert(oid, time);
                link.parents.insert(oid, Vec::new());
            });
            return Ok(Vec::new());
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
        let linkage = if let Some(reader) = self.single_pack.as_mut() {
            let object = reader.read_object(&commit_oid)?;
            if object.kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            crate::objects::parse_commit_linkage(&object.data)?
        } else {
            let object = self.repo.odb().read(&commit_oid)?;
            if object.kind != ObjectKind::Commit {
                return Err(Error::CorruptObject(format!(
                    "object {commit_oid} is not a commit"
                )));
            }
            crate::objects::parse_commit_linkage(&object.data)?
        };
        let time = crate::ident::committer_timestamp_for_until_filter(&linkage.committer);
        let parents: Vec<ObjectId> = if self.shallow_boundaries.contains(&commit_oid) {
            Vec::new()
        } else {
            linkage
                .parents
                .iter()
                .copied()
                .filter(|p| !self.promisor_stop.contains(p))
                .collect()
        };
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
    use crate::environment::RepositoryOptions;
    use crate::objects::ObjectKind;
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
                initial_branch: "main".to_owned(),
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
        let (ahead2, behind2, retained) = ahead_behind_of_target_commits(&repo, c3, c1, 1)?;
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
}
