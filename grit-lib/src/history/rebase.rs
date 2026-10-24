//! Stateless rebase onto a new base (`upstream..tip` replayed onto `onto`).

use std::collections::{HashMap, HashSet};

#[cfg(any(test, feature = "unstable-bench-helpers"))]
use std::cell::Cell;

use crate::diff::diff_trees;
use crate::merge_base::{
    commits_reachable_excluding_ancestors_of, is_ancestor, merge_bases_first_vs_rest,
};
use crate::merge_file::MergeFavor;
use crate::objects::{parse_commit, ObjectId};
use crate::patch_ids::{compute_patch_id, PatchIdMode};
use crate::repo::Repository;
use crate::revwalk::rev_list::topo_order_commit_subset;

use super::error::HistoryError;
use super::plan::{
    collect_scoped_refs, commit_parents, head_effect, plan_ref_updates, plan_rewrite_with_order,
    plan_rewrite_with_order_resumable, ResumablePlanOutcome,
};
use super::types::{
    CommitEdit, HistoryEdits, RefScope, RewriteOptions, RewritePlan, RewrittenCommit,
};

#[cfg(any(test, feature = "unstable-bench-helpers"))]
thread_local! { // hygiene: patch-id compute counter for tests and unstable-bench-helpers
    static PATCH_ID_COMPUTE_CALLS: Cell<usize> = const { Cell::new(0) };
}

/// Returns how many times patch-id computation ran on this thread since the last reset.
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[must_use]
pub fn test_patch_id_compute_call_count() -> usize {
    PATCH_ID_COMPUTE_CALLS.with(Cell::get)
}

/// Clears the patch-id computation counter on this thread (tests and benchmarks).
#[cfg(any(test, feature = "unstable-bench-helpers"))]
pub fn test_reset_patch_id_compute_call_count() {
    PATCH_ID_COMPUTE_CALLS.with(|c| c.set(0));
}

/// Inputs for [`plan_rebase`], matching `git rebase [--onto <base>] <upstream>` on a single branch tip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RebaseRequest {
    /// Commits reachable from the branch tip but not from this commit are replayed (Git's upstream argument).
    pub upstream: ObjectId,
    /// When `None`, replay onto [`RebaseRequest::upstream`]; otherwise onto this commit.
    pub onto: Option<ObjectId>,
    /// Tip of the branch being rebased (typically `HEAD`).
    pub branch_tip: ObjectId,
}

/// Result of [`plan_rebase`]: already current, a rewrite plan, or a conflict report (not an error).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebasePlan {
    /// The branch is already based on the effective onto commit with nothing to replay.
    UpToDate,
    /// Commits to replay and ref updates to apply.
    Plan(RewritePlan),
    /// Replay of a commit would conflict; the repository is unchanged.
    Conflict {
        /// Original commit that could not be replayed cleanly.
        commit: ObjectId,
        /// Repository-relative paths that conflicted.
        paths: Vec<String>,
    },
}

/// Options for [`rebase_onto`] and [`plan_rebase`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RebaseOntoOptions {
    /// Scope, empty-commit policy, and committer ident for replayed commits.
    pub rewrite: RewriteOptions,
    /// When `true` (default), omit commits whose stable patch-id already appears on the onto side between fork and onto.
    pub skip_applied: bool,
    /// When `true`, drop merge commits that are branch tips in the replay range and move refs to
    /// the replayed second parent (matches `git pull --rebase=merges` on linearizable merges).
    pub flatten_leaf_merges: bool,
    /// Auto-resolve replay conflicts the same way as `git rebase -X ours|theirs`.
    ///
    /// During replay, grit three-way-merges each commit's tree onto the new parent using the old
    /// parent as base. In that merge, **ours** is the onto (upstream) tree and **theirs** is the
    /// commit being replayed — matching Git's rebase implementation, not a branch merge where
    /// "ours" is the checked-out branch. [`MergeFavor::Ours`] therefore keeps upstream content;
    /// [`MergeFavor::Theirs`] keeps the replayed commit's changes.
    pub favor: MergeFavor,
}

impl RebaseOntoOptions {
    /// Default rebase options: update descendant branches, drop empty replays, skip applied patches.
    #[must_use]
    pub fn new(rewrite: RewriteOptions) -> Self {
        Self {
            rewrite,
            skip_applied: true,
            flatten_leaf_merges: false,
            favor: MergeFavor::default(),
        }
    }
}

/// Predicted outcome of [`plan_rebase_check`] (no ref, index, or worktree updates).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseCheckPlan {
    /// Nothing would be replayed.
    UpToDate,
    /// Every commit would replay cleanly; lists replacement commit OIDs in replay order.
    Clean {
        /// New commit object ids that would become branch tips after apply.
        commits: Vec<ObjectId>,
    },
    /// Replay would stop on this commit.
    Conflict {
        /// Original commit that could not be replayed cleanly.
        commit: ObjectId,
        /// Repository-relative paths that conflicted.
        paths: Vec<String>,
    },
}

/// Predict whether a rebase would succeed without updating refs, the index, or the worktree.
///
/// New commit objects may still be written to the object database (same as [`plan_rebase`]).
///
/// # Errors
///
/// Same planning errors as [`plan_rebase`], except conflicts are returned as
/// [`RebaseCheckPlan::Conflict`].
pub fn plan_rebase_check(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
) -> Result<RebaseCheckPlan, HistoryError> {
    match plan_rebase(repo, request, opts)? {
        RebasePlan::UpToDate => Ok(RebaseCheckPlan::UpToDate),
        RebasePlan::Conflict { commit, paths } => Ok(RebaseCheckPlan::Conflict { commit, paths }),
        RebasePlan::Plan(plan) => {
            let commits = plan.commits.iter().filter_map(|row| row.new).collect();
            Ok(RebaseCheckPlan::Clean { commits })
        }
    }
}

/// Plan a rebase with fork-point detection and bounded cherry patch-id lookup.
///
/// The replayed range is `merge-base(upstream, branch_tip)..branch_tip`, applied onto
/// `onto.unwrap_or(upstream)`, matching `git rebase [--onto X] U` on `branch_tip`.
///
/// # Errors
///
/// Returns [`HistoryError::Planning`] when tips or upstream are inconsistent, and other
/// planning failures except conflicts (those are returned as [`RebasePlan::Conflict`]).
pub fn plan_rebase(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
) -> Result<RebasePlan, HistoryError> {
    let fork = rebase_fork(repo, request.upstream, request.branch_tip)?;
    let effective_onto = request.onto.unwrap_or(request.upstream);
    if request.branch_tip == effective_onto {
        return Ok(RebasePlan::UpToDate);
    }
    let range = commits_in_rebase_range(repo, fork, &[request.branch_tip])?;
    if range.is_empty()
        && is_ancestor(repo, request.branch_tip, effective_onto).map_err(library_err)?
    {
        return Ok(RebasePlan::Plan(plan_rebase_fast_forward(
            repo,
            request.branch_tip,
            effective_onto,
            opts,
        )?));
    }
    if is_rebase_up_to_date(
        repo,
        fork,
        effective_onto,
        request.upstream,
        request.onto,
        request.branch_tip,
        &range,
    )? {
        return Ok(RebasePlan::UpToDate);
    }
    match rebase_onto_impl(
        repo,
        fork,
        effective_onto,
        &[request.branch_tip],
        opts,
        Some(&range),
    ) {
        Ok(plan) => Ok(RebasePlan::Plan(plan)),
        Err(HistoryError::Conflict { commit, paths }) => Ok(RebasePlan::Conflict { commit, paths }),
        Err(other) => Err(other),
    }
}

/// Plan replay of `upstream..tips` onto `onto` in topological order.
///
/// The `upstream` argument is the exclude boundary (fork point), not necessarily the user's
/// upstream ref; [`plan_rebase`] computes the fork from upstream and tip instead.
///
/// Merge commits in the range are replayed with updated parents and trees. Under
/// [`RefScope::Branches`](super::RefScope::Branches),
/// every local branch whose tip descends from the rewritten range is moved to its replayed tip.
///
/// # Errors
///
/// Returns [`HistoryError::Conflict`] and other planning
/// failures from [`crate::history::plan::plan_rewrite`].
pub fn rebase_onto(
    repo: &Repository,
    upstream: ObjectId,
    onto: ObjectId,
    tips: &[ObjectId],
    opts: &RebaseOntoOptions,
) -> Result<RewritePlan, HistoryError> {
    rebase_onto_impl(repo, upstream, onto, tips, opts, None)
}

pub(crate) fn rebase_fork(
    repo: &Repository,
    upstream: ObjectId,
    branch_tip: ObjectId,
) -> Result<ObjectId, HistoryError> {
    let bases = merge_bases_first_vs_rest(repo, branch_tip, &[upstream]).map_err(library_err)?;
    bases
        .into_iter()
        .next()
        .ok_or_else(|| HistoryError::Planning {
            message: format!(
                "no merge base between upstream {} and tip {}",
                upstream.to_hex(),
                branch_tip.to_hex()
            ),
        })
}

fn plan_rebase_fast_forward(
    repo: &Repository,
    branch_tip: ObjectId,
    onto: ObjectId,
    opts: &RebaseOntoOptions,
) -> Result<RewritePlan, HistoryError> {
    let scoped = collect_scoped_refs(repo, &opts.rewrite, branch_tip)?;
    let mapping = HashMap::from([(branch_tip, onto)]);
    let ref_updates = plan_ref_updates(&scoped, &mapping);
    let head = head_effect(repo, &mapping)?;
    Ok(RewritePlan {
        commits: vec![RewrittenCommit {
            old: branch_tip,
            new: Some(onto),
            change_id: None,
            subject: None,
        }],
        ref_updates,
        head,
    })
}

fn is_rebase_up_to_date(
    repo: &Repository,
    fork: ObjectId,
    effective_onto: ObjectId,
    upstream: ObjectId,
    onto: Option<ObjectId>,
    branch_tip: ObjectId,
    range: &[ObjectId],
) -> Result<bool, HistoryError> {
    if branch_tip == effective_onto {
        return Ok(true);
    }
    if range.is_empty() {
        return Ok(true);
    }
    // `git rebase <upstream>` (no `--onto`) leaves a linear branch unchanged when it already
    // extends upstream: merge-base(upstream, tip) == upstream and every commit in the range
    // is already on top of that base.
    if onto.is_none()
        && fork == upstream
        && is_ancestor(repo, upstream, branch_tip).map_err(library_err)?
    {
        return Ok(true);
    }
    Ok(false)
}

/// Build rebase [`HistoryEdits`], replay order, and commit range for `fork..tips` onto `onto`.
pub(crate) fn build_rebase_rewrite_inputs(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    tips: &[ObjectId],
    opts: &RebaseOntoOptions,
    range: Option<&[ObjectId]>,
) -> Result<(HistoryEdits, Vec<ObjectId>, Vec<ObjectId>), HistoryError> {
    if tips.is_empty() {
        return Err(HistoryError::Planning {
            message: "at least one tip commit is required".into(),
        });
    }

    for &tip in tips {
        if !is_ancestor(repo, fork, tip).map_err(library_err)? {
            return Err(HistoryError::Planning {
                message: format!(
                    "fork {} is not an ancestor of tip {}",
                    fork.to_hex(),
                    tip.to_hex()
                ),
            });
        }
    }

    let range = match range {
        Some(precomputed) => precomputed.to_vec(),
        None => commits_in_rebase_range(repo, fork, tips)?,
    };
    if range.is_empty() {
        return Ok((HistoryEdits::new(), Vec::new(), range));
    }

    let skip_map = if opts.skip_applied {
        applied_by_patch(repo, fork, onto, &range)?
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
            .all(|parent| *parent == fork || !range_set.contains(parent));
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

    let replay_order = extend_rebase_replay_order(repo, fork, &range, &opts.rewrite, &skip_map)?;
    Ok((edits, replay_order, range))
}

fn rebase_onto_impl(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    tips: &[ObjectId],
    opts: &RebaseOntoOptions,
    range: Option<&[ObjectId]>,
) -> Result<RewritePlan, HistoryError> {
    let (edits, replay_order, range) =
        build_rebase_rewrite_inputs(repo, fork, onto, tips, opts, range)?;
    if range.is_empty() {
        return Ok(RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: super::types::HeadEffect::default(),
        });
    }
    plan_rewrite_with_order(
        repo,
        &edits,
        &opts.rewrite,
        Some(&replay_order),
        opts.flatten_leaf_merges,
        opts.favor,
    )
}

/// Like [`plan_rebase`], but resumable planning may stop on the first replay conflict.
pub(crate) fn plan_rebase_resumable(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
) -> Result<Result<ResumablePlanOutcome, RebasePlan>, HistoryError> {
    let fork = rebase_fork(repo, request.upstream, request.branch_tip)?;
    let effective_onto = request.onto.unwrap_or(request.upstream);
    if request.branch_tip == effective_onto {
        return Ok(Err(RebasePlan::UpToDate));
    }
    let range = commits_in_rebase_range(repo, fork, &[request.branch_tip])?;
    if range.is_empty()
        && is_ancestor(repo, request.branch_tip, effective_onto).map_err(library_err)?
    {
        let plan = plan_rebase_fast_forward(repo, request.branch_tip, effective_onto, opts)?;
        return Ok(Ok(ResumablePlanOutcome::Complete(plan)));
    }
    if is_rebase_up_to_date(
        repo,
        fork,
        effective_onto,
        request.upstream,
        request.onto,
        request.branch_tip,
        &range,
    )? {
        return Ok(Err(RebasePlan::UpToDate));
    }
    let tips = [request.branch_tip];
    let (edits, replay_order, range) =
        build_rebase_rewrite_inputs(repo, fork, effective_onto, &tips, opts, Some(&range))?;
    if range.is_empty() {
        return Ok(Err(RebasePlan::UpToDate));
    }
    Ok(Ok(plan_rewrite_with_order_resumable(
        repo,
        &edits,
        &opts.rewrite,
        Some(&replay_order),
        opts.flatten_leaf_merges,
    )?))
}

/// Include sibling branch tips (same parent in the rebase range) when updating all descendant refs.
fn extend_rebase_replay_order(
    repo: &Repository,
    fork: ObjectId,
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
                .all(|parent| *parent == fork || in_order.contains(parent))
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
    fork: ObjectId,
    tips: &[ObjectId],
) -> Result<Vec<ObjectId>, HistoryError> {
    let mut commits =
        commits_reachable_excluding_ancestors_of(repo, tips, &[fork], None).map_err(library_err)?;
    commits.retain(|oid| *oid != fork);
    let set: HashSet<ObjectId> = commits.iter().copied().collect();
    for &tip in tips {
        if tip != fork && !set.contains(&tip) {
            commits.push(tip);
        }
    }
    let mut topo = topo_order_commit_subset(repo, &commits, true).map_err(library_err)?;
    // Topological oldest-first (topo_order_commit_subset is newest-first).
    topo.reverse();
    Ok(topo)
}

fn paths_changed_by_commit(
    repo: &Repository,
    commit: ObjectId,
) -> Result<HashSet<String>, HistoryError> {
    let obj = repo.odb().read(&commit).map_err(library_err)?;
    let parsed = parse_commit(&obj.data).map_err(library_err)?;
    let mut paths = HashSet::new();
    if parsed.parents.is_empty() {
        let empty = crate::diff::zero_oid();
        let changes =
            diff_trees(repo.odb(), Some(&empty), Some(&parsed.tree), "").map_err(library_err)?;
        for entry in changes {
            paths.insert(entry.path().to_string());
        }
        return Ok(paths);
    }
    for parent in &parsed.parents {
        let parent_obj = repo.odb().read(parent).map_err(library_err)?;
        let parent_parsed = parse_commit(&parent_obj.data).map_err(library_err)?;
        let changes = diff_trees(
            repo.odb(),
            Some(&parent_parsed.tree),
            Some(&parsed.tree),
            "",
        )
        .map_err(library_err)?;
        for entry in changes {
            paths.insert(entry.path().to_string());
        }
    }
    Ok(paths)
}

fn collect_range_touched_paths(
    repo: &Repository,
    range: &[ObjectId],
) -> Result<HashSet<String>, HistoryError> {
    let mut paths = HashSet::new();
    for &commit in range {
        paths.extend(paths_changed_by_commit(repo, commit)?);
    }
    Ok(paths)
}

fn commit_touches_any_path(
    repo: &Repository,
    commit: ObjectId,
    paths: &HashSet<String>,
) -> Result<bool, HistoryError> {
    if paths.is_empty() {
        return Ok(true);
    }
    let touched = paths_changed_by_commit(repo, commit)?;
    Ok(touched.iter().any(|p| paths.contains(p)))
}

/// Stable patch-ids for commits on `fork..onto`, optionally limited to commits touching `range_paths`.
fn patch_ids_in_fork_to_onto(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    range_paths: &HashSet<String>,
) -> Result<HashMap<ObjectId, ObjectId>, HistoryError> {
    if fork == onto {
        return Ok(HashMap::new());
    }
    let mut commits = commits_reachable_excluding_ancestors_of(repo, &[onto], &[fork], None)
        .map_err(library_err)?;
    commits.retain(|oid| *oid != fork);
    let mut commits = topo_order_commit_subset(repo, &commits, true).map_err(library_err)?;
    commits.reverse();
    let algo = repo.odb().hash_algo();
    let mut by_patch: HashMap<ObjectId, ObjectId> = HashMap::new();
    for oid in commits {
        if !range_paths.is_empty() && !commit_touches_any_path(repo, oid, range_paths)? {
            continue;
        }
        if let Ok(Some(pid)) =
            compute_patch_id_recorded(algo, repo.odb(), &oid, PatchIdMode::Stable)
        {
            by_patch.insert(pid, oid);
        }
    }
    Ok(by_patch)
}

fn compute_patch_id_recorded(
    algo: crate::objects::HashAlgo,
    odb: &crate::odb::Odb,
    commit_oid: &ObjectId,
    mode: PatchIdMode,
) -> Result<Option<ObjectId>, crate::error::Error> {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    PATCH_ID_COMPUTE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
    compute_patch_id(algo, odb, commit_oid, mode)
}

/// Map each commit in `range` to an existing commit in `fork..onto` with the same patch-id.
fn applied_by_patch(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    range: &[ObjectId],
) -> Result<HashMap<ObjectId, ObjectId>, HistoryError> {
    let range_paths = collect_range_touched_paths(repo, range)?;
    let by_patch = patch_ids_in_fork_to_onto(repo, fork, onto, &range_paths)?;
    let algo = repo.odb().hash_algo();
    let mut out = HashMap::new();
    for &commit in range {
        let Some(pid) = compute_patch_id_recorded(algo, repo.odb(), &commit, PatchIdMode::Stable)
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
        let map = applied_by_patch(&repo, root, onto_tip, &range).expect("map");
        assert_eq!(map.get(&replay), Some(&onto_tip));
    }

    #[test]
    fn patch_id_scan_bounded_on_large_onto_side() {
        test_reset_patch_id_compute_call_count();
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let fork_tree = blob_tree(&repo, "f.txt", b"base\n").expect("fork tree");
        let fork = write_commit(&repo, fork_tree, &[], "fork", &id).expect("fork");
        let mut parent = fork;
        for i in 0..120 {
            let noise = format!("noise/{i}.txt");
            let tree = blob_tree(&repo, &noise, format!("n{i}\n").as_bytes()).expect("noise tree");
            parent = write_commit(&repo, tree, &[parent], "noise", &id).expect("noise commit");
        }
        let onto = parent;
        let branch_tree = blob_tree(&repo, "f.txt", b"topic\n").expect("branch tree");
        let tip = write_commit(&repo, branch_tree, &[fork], "topic", &id).expect("tip");
        let range = commits_in_rebase_range(&repo, fork, &[tip]).expect("range");
        assert_eq!(range.len(), 1);
        let _map = applied_by_patch(&repo, fork, onto, &range).expect("applied");
        let calls = test_patch_id_compute_call_count();
        assert!(
            calls <= 4,
            "expected patch-id work proportional to fork..onto overlap, got {calls} calls"
        );
    }

    #[test]
    fn rebase_range_enumeration_is_bounded_on_deep_linear_history() {
        use crate::merge_base::{
            commits_reachable_excluding_ancestors_of,
            intersect_walk_test_hooks::{commit_parent_lookups, reset_commit_parent_lookups},
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let root_tree = blob_tree(&repo, "base.txt", b"base\n").expect("root tree");
        let root = write_commit(&repo, root_tree, &[], "root", &id).expect("root");
        let mut parent = root;
        for i in 0..400 {
            let path = format!("deep/{i}.txt");
            let tree = blob_tree(&repo, &path, format!("v{i}\n").as_bytes()).expect("deep tree");
            parent = write_commit(&repo, tree, &[parent], "deep", &id).expect("deep commit");
        }
        let fork = parent;
        let onto = fork;
        let mut tip = fork;
        for i in 0..20 {
            let path = format!("feat/{i}.txt");
            let tree = blob_tree(&repo, &path, format!("f{i}\n").as_bytes()).expect("feat tree");
            tip = write_commit(&repo, tree, &[tip], "feat", &id).expect("feat commit");
        }
        let emitted =
            commits_reachable_excluding_ancestors_of(&repo, &[tip], &[fork], None).expect("walk");
        assert_eq!(
            emitted.len(),
            20,
            "hide-aware walk must emit only fork..tip, not deep history below fork"
        );
        let range = commits_in_rebase_range(&repo, fork, &[tip]).expect("range");
        assert_eq!(range.len(), 20, "expected 20 commits in fork..tip");
        reset_commit_parent_lookups();
        let _map = applied_by_patch(&repo, root, onto, &range).expect("patch scan");
        let patch_lookups = commit_parent_lookups();
        assert!(
            patch_lookups <= 450,
            "fork..onto patch scan must stay proportional to onto depth, got {patch_lookups} lookups"
        );
    }

    #[test]
    fn rebase_range_honors_grafts_when_enumerating_fork_to_tip() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let root_tree = blob_tree(&repo, "root.txt", b"r\n").expect("root tree");
        let root = write_commit(&repo, root_tree, &[], "root", &id).expect("root");
        let mid_tree = blob_tree(&repo, "mid.txt", b"m\n").expect("mid tree");
        let mid = write_commit(&repo, mid_tree, &[root], "mid", &id).expect("mid");
        let tip_tree = blob_tree(&repo, "tip.txt", b"t\n").expect("tip tree");
        let tip = write_commit(&repo, tip_tree, &[mid], "tip", &id).expect("tip");
        let graft_path = repo.git_dir().join("info/grafts");
        std::fs::create_dir_all(graft_path.parent().expect("graft parent")).expect("graft dir");
        std::fs::write(&graft_path, format!("{} {}\n", tip.to_hex(), root.to_hex()))
            .expect("graft file");
        let range = commits_in_rebase_range(&repo, root, &[tip]).expect("range");
        assert_eq!(
            range,
            vec![tip],
            "grafted parent link must drop mid from fork..tip"
        );
    }

    #[test]
    fn plan_rebase_up_to_date_when_linear_child_of_upstream() {
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
        let upstream = write_commit(&repo, tree_a, &[], "first", &id).expect("first");
        let tree_b = blob_tree(&repo, "f.txt", b"b\n").expect("tree b");
        let tip = write_commit(&repo, tree_b, &[upstream], "second", &id).expect("second");
        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: crate::revwalk::history::EmptyPolicy::Drop,
            committer: id.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        let request = RebaseRequest {
            upstream,
            onto: None,
            branch_tip: tip,
        };
        let plan = plan_rebase(&repo, &request, &opts).expect("plan");
        assert_eq!(plan, RebasePlan::UpToDate);
    }

    #[test]
    fn patch_id_reuse_requires_full_patch_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let root_tree = blob_tree(&repo, "root.txt", b"r\n").expect("root tree");
        let root = write_commit(&repo, root_tree, &[], "root", &id).expect("root");
        let fork = root;
        let topic_tree = {
            let t1 = blob_tree(&repo, "a.txt", b"topic\n").expect("a");
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
                size: 5,
                oid: t1,
                flags: 5,
                flags_extended: None,
                path: b"a.txt".to_vec(),
                base_index_pos: 0,
            });
            write_tree_from_index(repo.odb(), &index, "").expect("topic tree")
        };
        let topic = write_commit(&repo, topic_tree, &[fork], "topic", &id).expect("topic");
        let onto_tree = {
            let a = blob_tree(&repo, "a.txt", b"topic\n").expect("a");
            let b = blob_tree(&repo, "b.txt", b"extra\n").expect("b");
            let mut index = Index::new();
            index.hash_algo = repo.odb().hash_algo();
            for (path, oid, size) in [(b"a.txt", a, 6usize), (b"b.txt", b, 6usize)] {
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
                    size: size as u32,
                    oid,
                    flags: path.len().min(0xfff) as u16,
                    flags_extended: None,
                    path: path.to_vec(),
                    base_index_pos: 0,
                });
            }
            write_tree_from_index(repo.odb(), &index, "").expect("onto tree")
        };
        let onto = write_commit(&repo, onto_tree, &[fork], "onto", &id).expect("onto");
        let range = vec![topic];
        let map = applied_by_patch(&repo, fork, onto, &range).expect("map");
        assert!(
            map.is_empty(),
            "partial path overlap must not reuse a commit with extra changes"
        );
    }

    #[test]
    fn plan_rebase_fast_forwards_when_tip_is_ancestor_of_upstream() {
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
        let first = write_commit(&repo, tree_a, &[], "first", &id).expect("first");
        let tree_b = blob_tree(&repo, "f.txt", b"b\n").expect("tree b");
        let second = write_commit(&repo, tree_b, &[first], "second", &id).expect("second");
        let tree_c = blob_tree(&repo, "f.txt", b"c\n").expect("tree c");
        let upstream = write_commit(&repo, tree_c, &[second], "third", &id).expect("third");
        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: crate::revwalk::history::EmptyPolicy::Drop,
            committer: id.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        std::fs::write(repo.git_dir().join("HEAD"), b"ref: refs/heads/main\n").expect("head");
        std::fs::write(
            repo.git_dir().join("refs/heads/main"),
            format!("{second}\n"),
        )
        .expect("main");
        let request = RebaseRequest {
            upstream,
            onto: None,
            branch_tip: second,
        };
        let plan = plan_rebase(&repo, &request, &opts).expect("plan");
        match plan {
            RebasePlan::Plan(rewrite) => {
                assert_eq!(rewrite.commits.len(), 1);
                assert_eq!(rewrite.commits[0].new, Some(upstream));
            }
            other => panic!("expected fast-forward plan, got {other:?}"),
        }
    }

    #[test]
    fn plan_rebase_up_to_date_when_at_upstream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let tree = blob_tree(&repo, "f.txt", b"a\n").expect("tree");
        let upstream = write_commit(&repo, tree, &[], "only", &id).expect("commit");
        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: crate::revwalk::history::EmptyPolicy::Drop,
            committer: id.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        let request = RebaseRequest {
            upstream,
            onto: None,
            branch_tip: upstream,
        };
        let plan = plan_rebase(&repo, &request, &opts).expect("plan");
        assert_eq!(plan, RebasePlan::UpToDate);
    }
}
