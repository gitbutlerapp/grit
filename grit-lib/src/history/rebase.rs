//! Stateless rebase onto a new base (`upstream..tip` replayed onto `onto`).

use std::collections::{HashMap, HashSet};

use crate::branch_tracking::upstream_tracking_full_ref;
use crate::commit_graph_file::{BloomPrecheck, CommitGraphChain};
use crate::config::ConfigSet;
use crate::diff::diff_trees;
use crate::diff::diff_trees_for_pathspecs;
use crate::hooks::run_pre_rebase;
use crate::merge_base::{
    commits_reachable_excluding_ancestors_of, is_ancestor, merge_bases_first_vs_rest,
    rebase_boundary_from_fork_point, rebase_fork_point_enabled, resolve_fork_point_reflog_ref,
    try_reflog_fork_point,
};
use crate::merge_file::MergeFavor;
use crate::objects::{parse_commit, ObjectId};
use crate::operation::Conflict;
use crate::patch_ids::{compute_patch_id, PatchIdMode};
use crate::refs::store::RefName;
use crate::repo::Repository;
use crate::revwalk::rev_list::topo_order_commit_subset;
use crate::state::{resolve_head_for_repository, HeadState};

use super::error::HistoryError;
use super::plan::{
    collect_scoped_refs, commit_parents, head_effect, plan_ref_updates, plan_rewrite_with_order,
    plan_rewrite_with_order_resumable, ResumablePlanOutcome,
};
use super::types::{
    CommitEdit, HistoryEdits, PlannedRefUpdate, RefScope, RewriteOptions, RewritePlan,
    RewrittenCommit,
};

#[cfg(any(test, feature = "test-support"))]
pub mod patch_id_inject {
    //! Test-support hook for counting patch-id computations during rebase.
    use std::cell::Cell;

    thread_local! { // hygiene: test-support/bench-only patch-id compute call counter
        static PATCH_ID_COMPUTE_CALLS: Cell<usize> = const { Cell::new(0) };
    }

    /// Returns how many times patch-id computation ran on this thread since the last reset.
    #[must_use]
    pub fn patch_id_compute_call_count() -> usize {
        PATCH_ID_COMPUTE_CALLS.with(Cell::get)
    }

    /// Clears the per-thread patch-id compute counter (for test isolation).
    pub fn reset_patch_id_compute_call_count() {
        PATCH_ID_COMPUTE_CALLS.with(|c| c.set(0));
    }

    pub(crate) fn record_patch_id_compute_call() {
        PATCH_ID_COMPUTE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
    }
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

/// Builds a [`RebaseRequest`] for pull/rebase and implicit-upstream `rebase` after fetch.
///
/// When `use_fork_point` is true, the exclude boundary comes from the upstream tracking ref's
/// reflog ([`try_reflog_fork_point`]) with Git's octopus fast-forward guard
/// ([`rebase_boundary_from_fork_point`]). Otherwise the merge base between `head` and
/// `upstream_tip` is used. A [`FetchedUpstream::landed`](crate::porcelain::pull::FetchedUpstream::landed)
/// boundary is composed when it is a descendant of the fork boundary.
///
/// # Errors
///
/// Propagates merge-base and reflog errors.
pub fn rebase_request_after_fetch(
    repo: &Repository,
    head: ObjectId,
    upstream_tip: ObjectId,
    upstream_reflog_ref: &str,
    landed: Option<ObjectId>,
    use_fork_point: bool,
) -> Result<RebaseRequest, HistoryError> {
    let merge_base = rebase_fork(repo, upstream_tip, head)?;
    if !use_fork_point {
        if let Some(base) = landed {
            if base != merge_base
                && is_ancestor(repo, base, head).map_err(history_repo_err)?
                && is_ancestor(repo, merge_base, base).map_err(history_repo_err)?
            {
                return Ok(RebaseRequest {
                    upstream: base,
                    onto: Some(upstream_tip),
                    branch_tip: head,
                });
            }
        }
        return Ok(RebaseRequest {
            upstream: upstream_tip,
            onto: None,
            branch_tip: head,
        });
    }
    let fork =
        try_reflog_fork_point(repo, upstream_reflog_ref, upstream_tip, head).map_err(|e| {
            HistoryError::Planning {
                message: e.to_string(),
            }
        })?;
    let mut boundary =
        rebase_boundary_from_fork_point(repo, head, upstream_tip, fork).map_err(|e| {
            HistoryError::Planning {
                message: e.to_string(),
            }
        })?;
    if let Some(base) = landed {
        if base != boundary
            && is_ancestor(repo, base, head).map_err(history_repo_err)?
            && is_ancestor(repo, merge_base, base).map_err(history_repo_err)?
        {
            boundary = pick_newer_boundary(repo, boundary, base)?;
        }
    }
    let onto = (boundary != upstream_tip).then_some(upstream_tip);
    Ok(RebaseRequest {
        upstream: boundary,
        onto,
        branch_tip: head,
    })
}

fn pick_newer_boundary(
    repo: &Repository,
    a: ObjectId,
    b: ObjectId,
) -> Result<ObjectId, HistoryError> {
    if is_ancestor(repo, a, b).map_err(history_repo_err)? && a != b {
        Ok(b)
    } else {
        Ok(a)
    }
}

fn history_repo_err(err: crate::error::Error) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
    }
}

/// Builds a [`RebaseRequest`] for `grit rebase` / `grit history rebase`.
///
/// When `explicit_onto` is set, `upstream` and `onto` are taken literally. Otherwise fork-point
/// rules match Git: enabled for implicit `@{upstream}` unless `rebase.forkPoint=false`, disabled
/// for an explicit upstream unless `use_fork_point` is true.
///
/// # Errors
///
/// Propagates revision, config, and planning errors.
pub fn rebase_run_request(
    repo: &Repository,
    branch_tip: ObjectId,
    upstream_oid: ObjectId,
    onto_oid: Option<ObjectId>,
    upstream_spec: Option<&str>,
    explicit_onto: bool,
    use_fork_point: Option<bool>,
) -> Result<RebaseRequest, HistoryError> {
    if explicit_onto {
        return Ok(RebaseRequest {
            upstream: upstream_oid,
            onto: onto_oid,
            branch_tip,
        });
    }
    let config = repo.config().snapshot().map_err(history_repo_err)?;
    let fork_enabled =
        resolve_rebase_use_fork_point(config.as_ref(), upstream_spec, use_fork_point);
    if fork_enabled {
        let reflog_ref = rebase_upstream_reflog_ref(repo, upstream_spec)?;
        rebase_request_after_fetch(repo, branch_tip, upstream_oid, &reflog_ref, None, true)
    } else {
        Ok(RebaseRequest {
            upstream: upstream_oid,
            onto: None,
            branch_tip,
        })
    }
}

/// Whether fork-point detection is active for this rebase invocation.
#[must_use]
pub fn resolve_rebase_use_fork_point(
    config: &ConfigSet,
    upstream_spec: Option<&str>,
    override_fork_point: Option<bool>,
) -> bool {
    if override_fork_point == Some(true) {
        return true;
    }
    if override_fork_point == Some(false) {
        return false;
    }
    if upstream_spec.is_some() {
        return false;
    }
    rebase_fork_point_enabled(config)
}

fn rebase_upstream_reflog_ref(
    repo: &Repository,
    upstream_spec: Option<&str>,
) -> Result<String, HistoryError> {
    if let Some(spec) = upstream_spec {
        return Ok(resolve_fork_point_reflog_ref(repo, spec));
    }
    let branch = match resolve_head_for_repository(repo).map_err(history_repo_err)? {
        HeadState::Branch { refname, .. } => refname
            .strip_prefix("refs/heads/")
            .map(str::to_owned)
            .ok_or_else(|| HistoryError::Planning {
                message: "cannot rebase from a non-branch HEAD".into(),
            })?,
        _ => {
            return Err(HistoryError::Planning {
                message: "cannot rebase detached HEAD without an upstream argument".into(),
            });
        }
    };
    upstream_tracking_full_ref(repo, &branch).ok_or_else(|| HistoryError::Planning {
        message: format!("no upstream configured for branch '{branch}'"),
    })
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
        /// First line of that commit's message.
        subject: String,
        /// Conflicted paths and how they conflicted, sorted by path.
        conflicts: Vec<Conflict>,
        /// Full names of the other branches whose replay failed, sorted; empty when the
        /// commit is on the branch being rebased. See [`HistoryError::Conflict`].
        refs: Vec<String>,
    },
}

/// Options for [`rebase_onto`] and [`plan_rebase`].
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RebaseOntoOptions {
    /// Scope, empty-commit policy, and committer ident for replayed commits.
    pub rewrite: RewriteOptions,
    /// When `true` (default), omit commits whose stable patch-id already appears on the onto side between fork and onto.
    pub skip_applied: bool,
    /// When `true`, linearize the replay the way `git rebase` without `--rebase-merges` does
    /// (what `git pull --rebase` runs): merge commits in the range are dropped and every other
    /// commit is replayed on top of the previous one, starting at the onto commit, in
    /// topological order. A branch that merged its upstream, or merged a local topic, keeps all
    /// of its own commits and ends on top of the onto commit. All tips are chained into that one
    /// line, so pass a single tip. When `false`, merges are replayed as merges (like
    /// `git rebase --rebase-merges`): only the first-parent chain from each tip to the fork
    /// is reparented onto `onto`; a side branch that forked at the rebase boundary is replayed
    /// onto the new base, while one that forked inside the range follows its rewritten parent.
    pub flatten_leaf_merges: bool,
    /// Auto-resolve replay conflicts the same way as `git rebase -X ours|theirs`.
    ///
    /// During replay, grit three-way-merges each commit's tree onto the new parent using the old
    /// parent as base. In that merge, **ours** is the onto (upstream) tree and **theirs** is the
    /// commit being replayed — matching Git's rebase implementation, not a branch merge where
    /// "ours" is the checked-out branch. [`MergeFavor::Ours`] therefore keeps upstream content;
    /// [`MergeFavor::Theirs`] keeps the replayed commit's changes.
    pub favor: MergeFavor,
    /// In a linear replay ([`Self::flatten_leaf_merges`]), commits of the range to leave out,
    /// as a dropped pick of `git rebase -i` is: the next commit is replayed onto the one before.
    /// A caller that knows a commit's change is already on the onto side as another commit,
    /// where a patch-id can't tell (a copy made on another base), lists it here. Ignored when
    /// merges are replayed. Read when the replay is planned; a rebase that stops keeps the
    /// drops in its saved plan, so the field is not saved with it.
    #[serde(skip)]
    pub drop: Vec<ObjectId>,
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
            drop: Vec::new(),
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
        /// First line of that commit's message.
        subject: String,
        /// Conflicted paths and how they conflicted, sorted by path.
        conflicts: Vec<Conflict>,
        /// Full names of the other branches whose replay failed, sorted; empty when the
        /// commit is on the branch being rebased. See [`HistoryError::Conflict`].
        refs: Vec<String>,
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
        RebasePlan::Conflict {
            commit,
            subject,
            conflicts,
            refs,
        } => Ok(RebaseCheckPlan::Conflict {
            commit,
            subject,
            conflicts,
            refs,
        }),
        RebasePlan::Plan(plan) => {
            let commits = plan.commits.iter().filter_map(|row| row.new).collect();
            Ok(RebaseCheckPlan::Clean { commits })
        }
    }
}

/// Whether a rebase runs the `pre-rebase` hook before it rewrites anything, and what the hook
/// is told.
///
/// The hook runs once the rebase is known to have work to do (never when the branch is already
/// up to date) and before any commit is replayed, so a refusal leaves nothing written. A
/// non-zero exit refuses the rebase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreRebase {
    /// Run the hook (Git's default).
    Run {
        /// The upstream exactly as the user named it (Git passes the argument, not the resolved
        /// commit); `None` when the branch's configured upstream was used, which the hook
        /// receives as its full ref name.
        upstream: Option<String>,
    },
    /// Do not run the hook (`--no-verify`).
    Skip,
}

impl PreRebase {
    pub(crate) fn run(&self, repo: &Repository) -> Result<(), HistoryError> {
        match self {
            Self::Run { upstream } => run_pre_rebase(repo, upstream.as_deref())
                .map(drop)
                .map_err(|err| HistoryError::from(crate::error::Error::Hook(err))),
            Self::Skip => Ok(()),
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
    plan_rebase_with_hook(repo, request, opts, &PreRebase::Skip)
}

/// [`plan_rebase`] for a rebase that is about to be applied: runs `pre_rebase` once the branch
/// is known not to be up to date, before any commit is replayed (Git's order), so a refusal
/// happens before a replay conflict is found or a new object is written.
///
/// # Errors
///
/// Same as [`plan_rebase`], plus [`HistoryError::Library`] wrapping
/// [`crate::error::Error::Hook`] when the hook refuses the rebase or cannot start.
pub fn plan_rebase_with_hook(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
    pre_rebase: &PreRebase,
) -> Result<RebasePlan, HistoryError> {
    let fork = rebase_fork(repo, request.upstream, request.branch_tip)?;
    let effective_onto = request.onto.unwrap_or(request.upstream);
    if rebase_keeps_branch(request, fork) {
        return Ok(RebasePlan::UpToDate);
    }
    let range = commits_in_rebase_range(repo, fork, &[request.branch_tip])?;
    if range.is_empty() && is_ancestor(repo, request.branch_tip, effective_onto)? {
        pre_rebase.run(repo)?;
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
    pre_rebase.run(repo)?;
    match rebase_onto_impl(
        repo,
        fork,
        effective_onto,
        &[request.branch_tip],
        opts,
        Some(&range),
    ) {
        Ok(plan) => Ok(RebasePlan::Plan(plan)),
        Err(HistoryError::Conflict {
            commit,
            subject,
            conflicts,
            refs,
        }) => Ok(RebasePlan::Conflict {
            commit,
            subject,
            conflicts,
            refs,
        }),
        Err(other) => Err(other),
    }
}

/// Plan replay of `upstream..tips` onto `onto` in topological order.
///
/// The `upstream` argument is the exclude boundary (fork point), not necessarily the user's
/// upstream ref; [`plan_rebase`] computes the fork from upstream and tip instead.
///
/// Merge commits in the range are replayed with updated parents and trees, unless
/// [`RebaseOntoOptions::flatten_leaf_merges`] linearizes the replay. Under
/// [`RefScope::Branches`](super::RefScope::Branches),
/// every local branch whose tip descends from the rewritten range is moved to its replayed tip,
/// except other branches at the same commit as `HEAD`, which keep the pre-rebase history.
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
    let bases = merge_bases_first_vs_rest(repo, branch_tip, &[upstream])?;
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
    let mut head = head_effect(repo, &mapping)?;
    head.checkout_onto = Some(onto);
    Ok(RewritePlan {
        commits: vec![RewrittenCommit {
            old: branch_tip,
            new: Some(onto),
            change_id: None,
            subject: None,
            reused: true,
        }],
        ref_updates,
        head,
    })
}

/// Whether the rebase leaves the branch as it is, decided before the replay range is walked:
/// the branch is already at the new base, or (without `--onto`) it already extends upstream,
/// which [`is_rebase_up_to_date`] would accept for any non-empty range.
fn rebase_keeps_branch(request: &RebaseRequest, fork: ObjectId) -> bool {
    let effective_onto = request.onto.unwrap_or(request.upstream);
    request.branch_tip == effective_onto || (request.onto.is_none() && fork == request.upstream)
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
    if onto.is_none() && fork == upstream && is_ancestor(repo, upstream, branch_tip)? {
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
        if !is_ancestor(repo, fork, tip)? {
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

    let edits = if opts.flatten_leaf_merges {
        linear_rebase_edits(repo, onto, &range, &skip_map, &opts.drop)?
    } else {
        merge_preserving_rebase_edits(repo, fork, onto, tips, &range, &skip_map)?
    };

    let mut replay_order =
        extend_rebase_replay_order(repo, fork, &range, &opts.rewrite, &skip_map)?;
    if !opts.flatten_leaf_merges {
        let range_set: HashSet<ObjectId> = range.iter().copied().collect();
        let mainline = mainline_commits_in_range(repo, fork, tips, &range_set)?;
        replay_order = priority_topo_replay_order(repo, &mainline, &replay_order)?;
    }
    Ok((edits, replay_order, range))
}

/// Edits that replay `range` in one line onto `onto`, like `git rebase` without
/// `--rebase-merges`.
///
/// Merges and the commits in `drop` are dropped, commits whose patch is already in
/// `fork..onto` reuse that commit, and every other commit is reparented onto the previous
/// replayed one. Planning with `linearize_onto` maps the dropped and reused entries to the tip
/// of the line.
fn linear_rebase_edits(
    repo: &Repository,
    onto: ObjectId,
    range: &[ObjectId],
    skip_map: &HashMap<ObjectId, ObjectId>,
    drop: &[ObjectId],
) -> Result<HistoryEdits, HistoryError> {
    let mut edits = HistoryEdits::new();
    let mut previous = onto;
    for &commit in range {
        if let Some(&existing) = skip_map.get(&commit) {
            edits.insert(
                commit,
                CommitEdit::Reuse {
                    existing,
                    parent: previous,
                },
            );
            continue;
        }
        if drop.contains(&commit) || commit_parents(repo, commit)?.len() > 1 {
            edits.insert(commit, CommitEdit::Drop);
            continue;
        }
        edits.insert(commit, CommitEdit::Reparent { parent: previous });
        previous = commit;
    }
    Ok(edits)
}

/// Edits that replay `range` onto `onto` keeping its shape: the first-parent chain from each
/// tip back to `fork` is reparented onto `onto` when its first parent is the fork or outside
/// the range (even if other merge parents are still in the range); side branches that forked
/// at the boundary (first parent is the fork) are reparented onto `onto`, and merge commits are
/// replayed with two parents.
///
/// A commit whose patch is already in `fork..onto` is skipped: what follows it continues from
/// where the commit itself would have been replayed onto, never from the upstream equivalent.
fn merge_preserving_rebase_edits(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    tips: &[ObjectId],
    range: &[ObjectId],
    skip_map: &HashMap<ObjectId, ObjectId>,
) -> Result<HistoryEdits, HistoryError> {
    let range_set: HashSet<ObjectId> = range.iter().copied().collect();
    let mainline = mainline_commits_in_range(repo, fork, tips, &range_set)?;
    let mut edits = HistoryEdits::new();
    for &commit in range {
        let parents = commit_parents(repo, commit)?;
        let first_parent = parents.first().copied();
        let reparent_onto = match first_parent {
            Some(fp) if mainline.contains(&commit) && fp == fork => Some(onto),
            Some(fp) if mainline.contains(&commit) && !range_set.contains(&fp) => Some(onto),
            Some(fp) if !mainline.contains(&commit) && fp == fork => Some(onto),
            _ => None,
        };
        if let Some(&existing) = skip_map.get(&commit) {
            let parent = reparent_onto.or(parents.first().copied()).unwrap_or(onto);
            edits.insert(commit, CommitEdit::Reuse { existing, parent });
            continue;
        }
        if let Some(parent) = reparent_onto {
            edits.insert(commit, CommitEdit::Reparent { parent });
        }
    }
    Ok(edits)
}

/// Stable priority topological replay order for merge-preserving rebases.
///
/// Every in-range parent is emitted before its child. Among commits whose in-range parents are
/// already scheduled, side branches run before mainline commits (so boundary topics replay before
/// a parallel mainline root), with ties broken by the incoming topological order.
fn priority_topo_replay_order(
    repo: &Repository,
    mainline: &HashSet<ObjectId>,
    replay_order: &[ObjectId],
) -> Result<Vec<ObjectId>, HistoryError> {
    if replay_order.is_empty() {
        return Ok(Vec::new());
    }
    let order_index: HashMap<ObjectId, usize> = replay_order
        .iter()
        .enumerate()
        .map(|(index, &oid)| (oid, index))
        .collect();
    let in_replay: HashSet<ObjectId> = order_index.keys().copied().collect();

    let mut pending_parents: HashMap<ObjectId, usize> = HashMap::new();
    let mut children: HashMap<ObjectId, Vec<ObjectId>> = HashMap::new();
    for &commit in replay_order {
        let parents = commit_parents(repo, commit)?;
        let in_range = parents
            .iter()
            .filter(|parent| in_replay.contains(parent))
            .count();
        pending_parents.insert(commit, in_range);
        for &parent in &parents {
            if in_replay.contains(&parent) {
                children.entry(parent).or_default().push(commit);
            }
        }
    }

    let prefer_side_branch = |left: &ObjectId, right: &ObjectId| {
        let left_mainline = mainline.contains(left);
        let right_mainline = mainline.contains(right);
        match (left_mainline, right_mainline) {
            (true, false) => std::cmp::Ordering::Greater,
            (false, true) => std::cmp::Ordering::Less,
            _ => order_index[left].cmp(&order_index[right]),
        }
    };

    let mut ready: Vec<ObjectId> = replay_order
        .iter()
        .copied()
        .filter(|commit| pending_parents.get(commit).copied().unwrap_or(0) == 0)
        .collect();
    let mut ordered = Vec::with_capacity(replay_order.len());
    while ordered.len() < replay_order.len() {
        let Some(next) = ready
            .iter()
            .min_by(|a, b| prefer_side_branch(a, b))
            .copied()
        else {
            return Err(HistoryError::Planning {
                message: "rebase replay order has a cycle among in-range parents".into(),
            });
        };
        ready.retain(|commit| commit != &next);
        ordered.push(next);
        if let Some(kids) = children.get(&next) {
            for &child in kids {
                if let Some(pending) = pending_parents.get_mut(&child) {
                    *pending -= 1;
                    if *pending == 0 {
                        ready.push(child);
                    }
                }
            }
        }
    }
    Ok(ordered)
}

/// First-parent commits from each rebase tip back to `fork` (exclusive).
fn mainline_commits_in_range(
    repo: &Repository,
    fork: ObjectId,
    tips: &[ObjectId],
    range_set: &HashSet<ObjectId>,
) -> Result<HashSet<ObjectId>, HistoryError> {
    let mut mainline = HashSet::new();
    for &tip in tips {
        let mut current = tip;
        while current != fork && range_set.contains(&current) {
            mainline.insert(current);
            let parents = commit_parents(repo, current)?;
            current = parents.first().copied().unwrap_or(fork);
        }
    }
    Ok(mainline)
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
    let mut plan = plan_rewrite_with_order(
        repo,
        &edits,
        &opts.rewrite,
        Some(&replay_order),
        opts.flatten_leaf_merges.then_some(onto),
        opts.favor,
    )?;
    plan.head.checkout_onto = Some(onto);
    Ok(plan)
}

/// Replay plan for `--exec` when the branch is already based on the upstream: reuse each commit
/// in the replay order unchanged so commands run without minting new OIDs until a command rewrites
/// one (stacked branch tips are included the same way as [`extend_rebase_replay_order`]).
pub(crate) fn plan_rebase_identity_exec(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    range: &[ObjectId],
    opts: &RebaseOntoOptions,
) -> Result<RewritePlan, HistoryError> {
    let skip_map = HashMap::new();
    let replay_order = extend_rebase_replay_order(repo, fork, range, &opts.rewrite, &skip_map)?;
    let mut edits = HistoryEdits::new();
    let mut previous = onto;
    for &commit in &replay_order {
        edits.insert(
            commit,
            CommitEdit::Reuse {
                existing: commit,
                parent: previous,
            },
        );
        previous = commit;
    }
    let mut plan = plan_rewrite_with_order(
        repo,
        &edits,
        &opts.rewrite,
        Some(&replay_order),
        Some(onto),
        opts.favor,
    )?;
    plan.head.checkout_onto = Some(onto);
    // Identity mapping leaves `plan_ref_updates` empty; seed every scoped ref so
    // [`super::exec::ExecRewrites::finish`] can remap stacked tips after amending commands.
    if let Some(&oldest) = replay_order.first() {
        let scoped = collect_scoped_refs(repo, &opts.rewrite, oldest)?;
        for (name, old) in scoped {
            if plan
                .ref_updates
                .iter()
                .any(|update| update.name.as_str() == name.as_str())
            {
                continue;
            }
            plan.ref_updates.push(PlannedRefUpdate {
                name: RefName::from_trusted(name),
                old,
                new: old,
            });
        }
        plan.ref_updates.sort_by(|a, b| a.name.cmp(&b.name));
    }
    Ok(plan)
}

/// Like [`plan_rebase_with_hook`], but resumable planning may stop on the first replay conflict.
pub(crate) fn plan_rebase_resumable(
    repo: &Repository,
    request: &RebaseRequest,
    opts: &RebaseOntoOptions,
    pre_rebase: &PreRebase,
) -> Result<Result<ResumablePlanOutcome, RebasePlan>, HistoryError> {
    let fork = rebase_fork(repo, request.upstream, request.branch_tip)?;
    let effective_onto = request.onto.unwrap_or(request.upstream);
    if rebase_keeps_branch(request, fork) {
        return Ok(Err(RebasePlan::UpToDate));
    }
    let range = commits_in_rebase_range(repo, fork, &[request.branch_tip])?;
    if range.is_empty() && is_ancestor(repo, request.branch_tip, effective_onto)? {
        pre_rebase.run(repo)?;
        let plan = plan_rebase_fast_forward(repo, request.branch_tip, effective_onto, opts)?;
        return Ok(Ok(ResumablePlanOutcome::Complete(Box::new(plan))));
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
    pre_rebase.run(repo)?;
    let tips = [request.branch_tip];
    let (edits, replay_order, range) =
        build_rebase_rewrite_inputs(repo, fork, effective_onto, &tips, opts, Some(&range))?;
    if range.is_empty() {
        return Ok(Err(RebasePlan::UpToDate));
    }
    let mut outcome = plan_rewrite_with_order_resumable(
        repo,
        &edits,
        &opts.rewrite,
        Some(&replay_order),
        opts.flatten_leaf_merges.then_some(effective_onto),
        opts.favor,
    )?;
    if let ResumablePlanOutcome::Complete(plan) = &mut outcome {
        plan.head.checkout_onto = Some(effective_onto);
    }
    Ok(Ok(outcome))
}

/// Append commits on scoped branch tips that descend from the rebase range but are not in it.
///
/// Stacked branches often carry more than one commit above the checked-out branch; those commits
/// must replay after the primary range so ref updates stay on the rewritten tips.
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
    let range_set: HashSet<ObjectId> = range.iter().copied().collect();
    let oldest = range[0];
    let scoped = collect_scoped_refs(repo, opts, oldest)?;

    let mut extra = HashSet::new();
    for (_, tip) in &scoped {
        if skip_map.contains_key(tip) || range_set.contains(tip) {
            continue;
        }
        extra.extend(stacked_commits_above_range(
            repo, *tip, &range_set, fork, skip_map,
        )?);
    }

    if extra.is_empty() {
        return Ok(range.to_vec());
    }

    let extra_vec: Vec<ObjectId> = extra.into_iter().collect();
    let mut appended = topo_order_commit_subset(repo, &extra_vec, true)?;
    appended.reverse();

    let mut order = range.to_vec();
    let mut in_order = range_set;
    for oid in appended {
        if in_order.insert(oid) {
            order.push(oid);
        }
    }
    Ok(order)
}

/// Commits reachable from `tip` that descend from the rewritten range but are not part of it.
///
/// Unrelated parents of a stacked merge (orphan roots, etc.) are excluded even when they appear
/// first in the parent list; those parents stay unmapped when the merge is replayed.
fn stacked_commits_above_range(
    repo: &Repository,
    tip: ObjectId,
    range_set: &HashSet<ObjectId>,
    fork: ObjectId,
    skip_map: &HashMap<ObjectId, ObjectId>,
) -> Result<Vec<ObjectId>, HistoryError> {
    let mut reachable = commits_reachable_excluding_ancestors_of(repo, &[tip], &[fork], None)?;
    reachable.retain(|oid| *oid != fork && !range_set.contains(oid) && !skip_map.contains_key(oid));
    reachable.retain(|oid| descends_from_rewritten_range(repo, *oid, range_set));
    Ok(reachable)
}

fn descends_from_rewritten_range(
    repo: &Repository,
    commit: ObjectId,
    range_set: &HashSet<ObjectId>,
) -> bool {
    range_set
        .iter()
        .any(|&range_commit| is_ancestor(repo, range_commit, commit).unwrap_or(false))
}

pub(crate) fn commits_in_rebase_range(
    repo: &Repository,
    fork: ObjectId,
    tips: &[ObjectId],
) -> Result<Vec<ObjectId>, HistoryError> {
    let mut commits = commits_reachable_excluding_ancestors_of(repo, tips, &[fork], None)?;
    commits.retain(|oid| *oid != fork);
    let set: HashSet<ObjectId> = commits.iter().copied().collect();
    for &tip in tips {
        if tip != fork && !set.contains(&tip) {
            commits.push(tip);
        }
    }
    let mut topo = topo_order_commit_subset(repo, &commits, true)?;
    // Topological oldest-first (topo_order_commit_subset is newest-first).
    topo.reverse();
    Ok(topo)
}

fn paths_changed_by_commit(
    repo: &Repository,
    commit: ObjectId,
) -> Result<HashSet<String>, HistoryError> {
    let obj = repo.odb().read(&commit)?;
    let parsed = parse_commit(&obj.data)?;
    let mut parent_trees = parsed
        .parents
        .iter()
        .map(|parent| Ok(Some(repo.odb().commit_tree(parent)?)))
        .collect::<Result<Vec<_>, HistoryError>>()?;
    if parent_trees.is_empty() {
        // A root commit adds its whole tree; `None` diffs against the empty tree.
        parent_trees.push(None);
    }
    let mut paths = HashSet::new();
    for parent_tree in parent_trees {
        let changes = diff_trees(repo.odb(), parent_tree.as_ref(), Some(&parsed.tree), "")?;
        paths.extend(changes.iter().map(|entry| entry.path().to_string()));
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

/// Repository paths from a replay range plus pathspec spellings safe for diff/Bloom APIs.
struct ReplayPathFilters {
    repo_paths: Vec<String>,
    pathspecs: Vec<String>,
}

fn replay_path_filters(range_paths: &HashSet<String>) -> ReplayPathFilters {
    let repo_paths: Vec<String> = range_paths.iter().cloned().collect();
    let pathspecs: Vec<String> = repo_paths
        .iter()
        .map(|p| crate::pathspec::repository_path_as_literal_pathspec(p))
        .collect();
    ReplayPathFilters {
        repo_paths,
        pathspecs,
    }
}

fn commit_touches_replay_paths(
    repo: &Repository,
    commit: ObjectId,
    filters: &ReplayPathFilters,
    bloom_chain: Option<&CommitGraphChain>,
) -> Result<bool, HistoryError> {
    if filters.pathspecs.is_empty() {
        return Ok(true);
    }
    let obj = repo.odb().read(&commit)?;
    let parsed = parse_commit(&obj.data)?;
    let parents = &parsed.parents;
    if parents.len() > 1 {
        for parent in parents {
            let parent_tree = repo.odb().commit_tree(parent)?;
            if trees_differ_on_pathspecs(
                repo,
                Some(&parent_tree),
                Some(&parsed.tree),
                filters,
                bloom_chain,
                commit,
                false,
            )? {
                return Ok(true);
            }
        }
        return Ok(false);
    }
    let parent_tree = if parents.is_empty() {
        None
    } else {
        Some(repo.odb().commit_tree(&parents[0])?)
    };
    trees_differ_on_pathspecs(
        repo,
        parent_tree.as_ref(),
        Some(&parsed.tree),
        filters,
        bloom_chain,
        commit,
        true,
    )
}

fn trees_differ_on_pathspecs(
    repo: &Repository,
    old_tree: Option<&ObjectId>,
    new_tree: Option<&ObjectId>,
    filters: &ReplayPathFilters,
    bloom_chain: Option<&CommitGraphChain>,
    commit_oid: ObjectId,
    bloom_first_parent: bool,
) -> Result<bool, HistoryError> {
    if old_tree == new_tree && old_tree.is_some() {
        return Ok(false);
    }
    if bloom_first_parent {
        if let Some(chain) = bloom_chain {
            if let Ok(BloomPrecheck::DefinitelyNot) = chain.bloom_precheck_for_paths(
                repo.odb(),
                commit_oid,
                &filters.pathspecs,
                None,
                -1,
                true,
            ) {
                return Ok(false);
            }
        }
    }
    let entries =
        diff_trees_for_pathspecs(repo.odb(), old_tree, new_tree, &filters.pathspecs, None)
            .map_err(history_repo_err)?;
    Ok(!entries.is_empty())
}

/// True when every replay path is absent from the commit's changed-path Bloom filter.
fn replay_paths_bloom_definitely_unchanged(
    chain: &CommitGraphChain,
    commit_oid: ObjectId,
    repo_paths: &[String],
) -> bool {
    use crate::bloom::bloom_keyvec_for_path;
    let Some(settings) = chain.bloom_settings_for_commit(&commit_oid) else {
        return false;
    };
    let mut checked_any = false;
    for path in repo_paths {
        if path.is_empty() {
            continue;
        }
        let keys = bloom_keyvec_for_path(path.as_str(), &settings);
        if keys.is_empty() {
            return false;
        }
        checked_any = true;
        if !chain.bloom_definitely_not_for_keys(commit_oid, &keys) {
            return false;
        }
    }
    checked_any
}

/// Stable patch-ids for commits on `fork..onto`, limited to `needed_pids` and path filters.
fn patch_ids_in_fork_to_onto(
    repo: &Repository,
    fork: ObjectId,
    onto: ObjectId,
    range_paths: &HashSet<String>,
    needed_pids: &HashSet<ObjectId>,
) -> Result<HashMap<ObjectId, ObjectId>, HistoryError> {
    if fork == onto || needed_pids.is_empty() {
        return Ok(HashMap::new());
    }
    let mut commits = commits_reachable_excluding_ancestors_of(repo, &[onto], &[fork], None)?;
    commits.retain(|oid| *oid != fork);
    let mut commits = topo_order_commit_subset(repo, &commits, true)?;
    commits.reverse();
    let filters = replay_path_filters(range_paths);
    let bloom_chain = repo.odb().commit_graph_chain();
    let bloom_ref = bloom_chain.as_deref();
    let algo = repo.odb().hash_algo();
    let mut by_patch: HashMap<ObjectId, ObjectId> = HashMap::new();
    for oid in commits {
        if !filters.pathspecs.is_empty() {
            if bloom_ref.is_some_and(|chain| {
                replay_paths_bloom_definitely_unchanged(chain, oid, &filters.repo_paths)
            }) {
                continue;
            }
            if !commit_touches_replay_paths(repo, oid, &filters, bloom_ref)? {
                continue;
            }
        }
        if let Ok(Some(pid)) =
            compute_patch_id_recorded(algo, repo.odb(), &oid, PatchIdMode::Stable)
        {
            if needed_pids.contains(&pid) {
                by_patch.insert(pid, oid);
            }
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
    #[cfg(any(test, feature = "test-support"))]
    patch_id_inject::record_patch_id_compute_call();
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
    let algo = repo.odb().hash_algo();
    let mut needed_pids = HashSet::new();
    let mut range_patch_ids: HashMap<ObjectId, ObjectId> = HashMap::new();
    for &commit in range {
        let Some(pid) = compute_patch_id_recorded(algo, repo.odb(), &commit, PatchIdMode::Stable)?
        else {
            continue;
        };
        needed_pids.insert(pid);
        range_patch_ids.insert(commit, pid);
    }
    if needed_pids.is_empty() {
        return Ok(HashMap::new());
    }
    let by_patch = patch_ids_in_fork_to_onto(repo, fork, onto, &range_paths, &needed_pids)?;
    let mut out = HashMap::new();
    for (&commit, &pid) in &range_patch_ids {
        if let Some(&existing) = by_patch.get(&pid) {
            out.insert(commit, existing);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::Error;
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
        write_tree_from_index(repo.odb(), &index, "").map_err(Error::from)
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
        let (repo, _) = Repository::init(
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
    fn patch_lookup_diffs_merged_unrelated_root_against_empty_tree() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let fork_tree = blob_tree(&repo, "f", b"base\n").expect("fork tree");
        let fork = write_commit(&repo, fork_tree, &[], "base", &id).expect("fork");
        let imported_tree = blob_tree(&repo, "g", b"imported\n").expect("imported tree");
        let imported =
            write_commit(&repo, imported_tree, &[], "imported root", &id).expect("imported");
        let merge_tree =
            files_tree(&repo, &[("f", b"base\n"), ("g", b"imported\n")]).expect("merge tree");
        let onto = write_commit(&repo, merge_tree, &[fork, imported], "merge", &id).expect("merge");
        let feat_tree = blob_tree(&repo, "f", b"base\nfeat\n").expect("feat tree");
        let tip = write_commit(&repo, feat_tree, &[fork], "feat", &id).expect("tip");

        assert_eq!(
            paths_changed_by_commit(&repo, imported).expect("root paths"),
            HashSet::from(["g".to_owned()])
        );
        let range = commits_in_rebase_range(&repo, fork, &[tip]).expect("range");
        assert_eq!(range, vec![tip]);
        let map = applied_by_patch(&repo, fork, onto, &range).expect("applied");
        assert!(map.is_empty(), "feat has no equivalent on the onto side");
        std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("main");
        match plan_rebase(
            &repo,
            &RebaseRequest {
                upstream: onto,
                onto: None,
                branch_tip: tip,
            },
            &RebaseOntoOptions::new(RewriteOptions {
                scope: RefScope::Head,
                empty: crate::revwalk::history::EmptyPolicy::Drop,
                committer: id.parse().expect("signature"),
                preserve_committer: false,
                sign: false,
            }),
        )
        .expect("plan_rebase")
        {
            RebasePlan::Plan(plan) => assert_eq!(plan.commits.len(), 1),
            other => panic!("expected a one-commit plan, got {other:?}"),
        }
    }

    #[test]
    fn patch_id_scan_bounded_on_large_onto_side() {
        patch_id_inject::reset_patch_id_compute_call_count();
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let fork_tree = files_tree(
            &repo,
            &[
                ("f.txt", b"base\n"),
                ("wide/shared/a.txt", b"a\n"),
                ("wide/shared/b.txt", b"b\n"),
                ("wide/shared/c.txt", b"c\n"),
            ],
        )
        .expect("fork tree");
        let fork = write_commit(&repo, fork_tree, &[], "fork", &id).expect("fork");
        let mut parent = fork;
        for i in 0..120 {
            let noise = format!("noise/{i}.txt");
            parent = commit_with_extra_file(
                &repo,
                parent,
                &noise,
                format!("n{i}\n").as_bytes(),
                "noise",
                &id,
            )
            .expect("noise commit");
        }
        let onto = parent;
        let tip =
            commit_with_extra_file(&repo, fork, "f.txt", b"topic\n", "topic", &id).expect("tip");
        let range = commits_in_rebase_range(&repo, fork, &[tip]).expect("range");
        assert_eq!(range.len(), 1);
        use crate::merge_base::intersect_walk_test_hooks::{
            commit_parent_lookups, reset_commit_parent_lookups,
        };
        reset_commit_parent_lookups();
        let _map = applied_by_patch(&repo, fork, onto, &range).expect("applied");
        let calls = patch_id_inject::patch_id_compute_call_count();
        assert!(
            calls <= 4,
            "expected patch-id work proportional to fork..onto overlap, got {calls} calls"
        );
        let patch_lookups = commit_parent_lookups();
        assert!(
            patch_lookups <= 450,
            "prefilter must not walk every onto commit parent, got {patch_lookups} lookups"
        );
    }

    fn commit_with_extra_file(
        repo: &Repository,
        parent: ObjectId,
        path: &str,
        content: &[u8],
        msg: &str,
        id: &str,
    ) -> Result<ObjectId, HistoryError> {
        use crate::porcelain::index_from_tree::index_from_tree;
        let parent_tree = repo.odb().commit_tree(&parent).map_err(history_repo_err)?;
        let mut index = index_from_tree(repo, &parent_tree).map_err(history_repo_err)?;
        let blob = repo
            .odb()
            .write(ObjectKind::Blob, content)
            .map_err(history_repo_err)?;
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
            size: content.len() as u32,
            oid: blob,
            flags: path.len().min(0xfff) as u16,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        });
        let tree = write_tree_from_index(repo.odb(), &index, "")
            .map_err(|e| history_repo_err(Error::from(e)))?;
        write_commit(repo, tree, &[parent], msg, id).map_err(history_repo_err)
    }

    fn colon_literal_skip_applied_fixture(
        repo: &Repository,
        id: &str,
    ) -> (ObjectId, ObjectId, ObjectId, ObjectId) {
        const MAGIC: &str = ":(literal)foo";
        let base_tree = blob_tree(repo, "base", b"base\n").expect("base tree");
        let fork = write_commit(repo, base_tree, &[], "fork", id).expect("fork");
        let add_tree =
            files_tree(repo, &[("base", b"base\n"), (MAGIC, b"topic\n")]).expect("add tree");
        let add = write_commit(repo, add_tree, &[fork], "add topic", id).expect("add");
        let revert_tree = blob_tree(repo, "base", b"base\n").expect("revert tree");
        let onto = write_commit(repo, revert_tree, &[add], "revert", id).expect("onto");
        let topic_tree =
            files_tree(repo, &[("base", b"base\n"), (MAGIC, b"topic\n")]).expect("topic tree");
        let tip = write_commit(repo, topic_tree, &[fork], "topic", id).expect("tip");
        (fork, add, onto, tip)
    }

    #[test]
    fn skip_applied_reuses_patch_for_colon_literal_filename_without_graph() {
        skip_applied_reuses_patch_for_colon_literal_filename(false);
    }

    #[test]
    fn skip_applied_reuses_patch_for_colon_literal_filename_with_commit_graph() {
        skip_applied_reuses_patch_for_colon_literal_filename(true);
    }

    fn skip_applied_reuses_patch_for_colon_literal_filename(write_graph: bool) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repo(dir.path());
        let id = ident(1);
        let (fork, add, onto, tip) = colon_literal_skip_applied_fixture(&repo, &id);
        if write_graph {
            repo.odb().write_commit_graph().expect("commit-graph");
        }
        let range = commits_in_rebase_range(&repo, fork, &[tip]).expect("range");
        let map = applied_by_patch(&repo, fork, onto, &range).expect("applied");
        assert_eq!(
            map.get(&tip),
            Some(&add),
            "identical patch on onto side must be found for colon-leading paths"
        );
        std::fs::write(repo.git_dir().join("refs/heads/main"), format!("{tip}\n")).expect("ref");
        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: crate::revwalk::history::EmptyPolicy::Drop,
            committer: id.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        let plan = plan_rebase(
            &repo,
            &RebaseRequest {
                upstream: fork,
                onto: Some(onto),
                branch_tip: tip,
            },
            &opts,
        )
        .expect("plan");
        let RebasePlan::Plan(rewrite) = plan else {
            panic!("expected a replay plan");
        };
        assert_eq!(rewrite.commits.len(), 1);
        assert_eq!(rewrite.commits[0].old, tip);
        assert_eq!(rewrite.commits[0].new, Some(add));
        assert!(
            rewrite.commits[0].reused,
            "topic commit must reuse onto add"
        );
    }

    #[test]
    fn rebase_range_enumeration_is_bounded_on_deep_linear_history() {
        use crate::merge_base::{
            commits_reachable_excluding_ancestors_of,
            intersect_walk_test_hooks::{commit_parent_lookups, reset_commit_parent_lookups},
        };

        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
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
        let (repo, _) = Repository::init(
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
        let (repo, _) = Repository::init(
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
    fn plan_rebase_up_to_date_when_branch_extends_upstream_through_a_merge() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let tree_a = blob_tree(&repo, "f.txt", b"a\n").expect("tree");
        let base = write_commit(&repo, tree_a, &[], "base", &id).expect("base");
        let mut side = base;
        for i in 0..20 {
            let tree = blob_tree(&repo, "side.txt", format!("{i}\n").as_bytes()).expect("tree");
            side = write_commit(&repo, tree, &[side], "side", &id).expect("side");
        }
        let upstream = write_commit(&repo, tree_a, &[base], "upstream", &id).expect("upstream");
        let tip = write_commit(&repo, tree_a, &[upstream, side], "merge", &id).expect("merge");
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
        assert_eq!(
            plan_rebase(&repo, &request, &opts).expect("plan"),
            RebasePlan::UpToDate
        );
    }

    #[test]
    fn patch_id_reuse_requires_full_patch_match() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
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
        let (repo, _) = Repository::init(
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
        let (repo, _) = Repository::init(
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

    fn files_tree(repo: &Repository, files: &[(&str, &[u8])]) -> crate::error::Result<ObjectId> {
        let mut index = Index::new();
        index.hash_algo = repo.odb().hash_algo();
        for (path, bytes) in files {
            let oid = repo.odb().write(ObjectKind::Blob, bytes)?;
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
                oid,
                flags: path.len().min(0xfff) as u16,
                flags_extended: None,
                path: path.as_bytes().to_vec(),
                base_index_pos: 0,
            });
        }
        write_tree_from_index(repo.odb(), &index, "").map_err(Error::from)
    }

    fn init_repo(dir: &std::path::Path) -> Repository {
        let (repo, _) = Repository::init(
            dir,
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        repo
    }

    fn flatten_opts(id: &str) -> RebaseOntoOptions {
        let mut opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: crate::revwalk::history::EmptyPolicy::Drop,
            committer: id.parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        opts.flatten_leaf_merges = true;
        opts
    }

    /// `root`, local `a` on it, upstream `s2` on it, `m` merging `s2` into `a`, upstream `s3`
    /// on `s2`; `refs/heads/main` (checked out) points at `m`.
    struct MergedUpstream {
        a: ObjectId,
        s2: ObjectId,
        m: ObjectId,
        s3: ObjectId,
    }

    fn merged_upstream_fixture(repo: &Repository, id: &str) -> MergedUpstream {
        let root_tree = files_tree(repo, &[("base.txt", b"base\n")]).expect("root tree");
        let root = write_commit(repo, root_tree, &[], "root", id).expect("root");
        let a_tree =
            files_tree(repo, &[("a.txt", b"local\n"), ("base.txt", b"base\n")]).expect("a tree");
        let a = write_commit(repo, a_tree, &[root], "Change a", id).expect("a");
        let s2_tree =
            files_tree(repo, &[("b.txt", b"2\n"), ("base.txt", b"base\n")]).expect("s2 tree");
        let s2 = write_commit(repo, s2_tree, &[root], "S2", id).expect("s2");
        let m_tree = files_tree(
            repo,
            &[
                ("a.txt", b"local\n"),
                ("b.txt", b"2\n"),
                ("base.txt", b"base\n"),
            ],
        )
        .expect("m tree");
        let m = write_commit(repo, m_tree, &[a, s2], "Merge S2", id).expect("m");
        let s3_tree = files_tree(
            repo,
            &[
                ("b.txt", b"2\n"),
                ("base.txt", b"base\n"),
                ("c.txt", b"3\n"),
            ],
        )
        .expect("s3 tree");
        let s3 = write_commit(repo, s3_tree, &[s2], "S3", id).expect("s3");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &m).expect("main");
        MergedUpstream { a, s2, m, s3 }
    }

    #[test]
    fn linear_rebase_edits_chain_commits_and_drop_merges() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repo(dir.path());
        let id = ident(1);
        let fx = merged_upstream_fixture(&repo, &id);
        let range = commits_in_rebase_range(&repo, fx.s2, &[fx.m]).expect("range");
        assert_eq!(range, vec![fx.a, fx.m]);
        let edits = linear_rebase_edits(&repo, fx.s3, &range, &HashMap::new(), &[]).expect("edits");
        assert_eq!(
            edits.get(&fx.a),
            Some(&CommitEdit::Reparent { parent: fx.s3 })
        );
        assert_eq!(edits.get(&fx.m), Some(&CommitEdit::Drop));
    }

    #[test]
    fn a_linear_rebase_leaves_out_the_commits_it_is_told_to_drop() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repo(dir.path());
        let id = ident(1);
        let base = files_tree(&repo, &[("base.txt", b"base\n")]).expect("tree");
        let root = write_commit(&repo, base, &[], "root", &id).expect("root");
        let on_root = |files: &[(&str, &[u8])], parent: ObjectId, msg: &str| {
            let mut all: Vec<(&str, &[u8])> = vec![("base.txt", b"base\n")];
            all.extend_from_slice(files);
            let tree = files_tree(&repo, &all).expect("tree");
            write_commit(&repo, tree, &[parent], msg, &id).expect("commit")
        };
        let a = on_root(&[("a.txt", b"a\n")], root, "a");
        // A copy of upstream's u, with other content than u's patch, so no patch-id matches.
        let copy = on_root(&[("a.txt", b"a\n"), ("u.txt", b"mine\n")], a, "copy of u");
        let c = on_root(
            &[("a.txt", b"a\n"), ("u.txt", b"mine\n"), ("c.txt", b"c\n")],
            copy,
            "c",
        );
        let onto = on_root(&[("u.txt", b"u\n")], root, "u");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &c).expect("main");
        let mut opts = flatten_opts(&id);
        opts.drop = vec![copy];

        let plan = rebase_onto(&repo, root, onto, &[c], &opts).expect("plan");
        let row = |old: ObjectId| {
            plan.commits
                .iter()
                .find(|row| row.old == old)
                .expect("row")
                .new
        };
        assert_eq!(row(copy), None, "the copy is left out");
        let new_a = row(a).expect("a is replayed");
        let new_c = row(c).expect("c is replayed");
        let replayed = repo.odb().read_commit(&new_c).expect("commit");
        assert_eq!(replayed.parents, vec![new_a], "c follows a");
        assert_eq!(
            replayed.tree,
            files_tree(
                &repo,
                &[
                    ("a.txt", b"a\n"),
                    ("base.txt", b"base\n"),
                    ("c.txt", b"c\n"),
                    ("u.txt", b"u\n"),
                ],
            )
            .expect("tree"),
            "upstream's u stays as upstream has it"
        );
        assert_eq!(plan.ref_updates[0].new, new_c);
    }

    #[test]
    fn a_linear_rebase_replays_across_a_directory_that_became_a_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repo(dir.path());
        let id = ident(1);
        let commit = |files: &[(&str, &[u8])], parents: &[ObjectId], msg: &str| {
            let tree = files_tree(&repo, files).expect("tree");
            write_commit(&repo, tree, parents, msg, &id).expect("commit")
        };
        let a: (&str, &[u8]) = ("src/a.rs", b"a\n");
        let b: (&str, &[u8]) = ("src/b.rs", b"b\n");
        let lib_dir: (&str, &[u8]) = ("src/lib/x.rs", b"x\n");
        let lib_file: (&str, &[u8]) = ("src/lib", b"now a file\n");
        let root = commit(&[a, lib_dir], &[], "root");
        let made_file = commit(&[a, lib_file], &[root], "src/lib becomes a file");
        let added_b = commit(&[a, b, lib_dir], &[root], "add b");
        let expected = files_tree(&repo, &[a, b, lib_file]).expect("tree");

        // Local work that never touches src/lib, onto upstream that made it a file, and local
        // work that made it a file, onto upstream that left it alone.
        for (local, onto) in [(added_b, made_file), (made_file, added_b)] {
            crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &local).expect("main");
            let plan = rebase_onto(&repo, root, onto, &[local], &flatten_opts(&id))
                .expect("a clean replay");
            let new = plan
                .commits
                .iter()
                .find(|row| row.old == local)
                .and_then(|row| row.new)
                .expect("replayed");
            let replayed = repo.odb().read_commit(&new).expect("commit");
            assert_eq!(replayed.parents, vec![onto]);
            assert_eq!(replayed.tree, expected, "src/lib is the file, b.rs is kept");
        }
    }

    #[test]
    fn flattened_rebase_keeps_local_commit_under_merge_of_upstream() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repo(dir.path());
        let id = ident(1);
        let fx = merged_upstream_fixture(&repo, &id);

        let plan = rebase_onto(&repo, fx.s2, fx.s3, &[fx.m], &flatten_opts(&id)).expect("plan");

        let row = |old: ObjectId| {
            plan.commits
                .iter()
                .find(|row| row.old == old)
                .expect("row")
                .new
        };
        assert_eq!(row(fx.m), None, "the merge is dropped");
        let replayed = row(fx.a).expect("local commit is replayed");
        let commit = repo.odb().read_commit(&replayed).expect("replayed commit");
        assert_eq!(commit.parents, vec![fx.s3]);
        let expected_tree = files_tree(
            &repo,
            &[
                ("a.txt", b"local\n"),
                ("b.txt", b"2\n"),
                ("base.txt", b"base\n"),
                ("c.txt", b"3\n"),
            ],
        )
        .expect("expected tree");
        assert_eq!(commit.tree, expected_tree);
        assert_eq!(plan.ref_updates.len(), 1);
        assert_eq!(
            plan.ref_updates[0].name,
            RefName::from_trusted("refs/heads/main")
        );
        assert_eq!(
            plan.ref_updates[0].new, replayed,
            "the branch lands on the replayed local commit, not the merged-in upstream commit"
        );
    }

    #[test]
    fn flattened_rebase_of_merge_without_local_commits_lands_on_onto() {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = init_repo(dir.path());
        let id = ident(1);
        let root_tree = files_tree(&repo, &[("base.txt", b"base\n")]).expect("root tree");
        let root = write_commit(&repo, root_tree, &[], "root", &id).expect("root");
        let s2_tree =
            files_tree(&repo, &[("b.txt", b"2\n"), ("base.txt", b"base\n")]).expect("s2 tree");
        let s2 = write_commit(&repo, s2_tree, &[root], "S2", &id).expect("s2");
        let m = write_commit(&repo, s2_tree, &[root, s2], "Merge S2", &id).expect("m");
        let s3_tree = files_tree(
            &repo,
            &[
                ("b.txt", b"2\n"),
                ("base.txt", b"base\n"),
                ("c.txt", b"3\n"),
            ],
        )
        .expect("s3 tree");
        let s3 = write_commit(&repo, s3_tree, &[s2], "S3", &id).expect("s3");
        crate::refs::write_ref(repo.git_dir(), "refs/heads/main", &m).expect("main");

        let plan = rebase_onto(&repo, s2, s3, &[m], &flatten_opts(&id)).expect("plan");

        assert_eq!(plan.ref_updates.len(), 1);
        assert_eq!(plan.ref_updates[0].new, s3);
    }

    #[test]
    fn priority_topo_replay_order_sequential_topic_merges_issue_2365() {
        let dir = tempfile::tempdir().expect("tempdir");
        let (repo, _) = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let id = ident(1);
        let fork_tree = blob_tree(&repo, "base", b"base\n").expect("fork tree");
        let fork = write_commit(&repo, fork_tree, &[], "base", &id).expect("fork");
        let t1_tree = blob_tree(&repo, "t1.txt", b"t1\n").expect("t1 tree");
        let t1 = write_commit(&repo, t1_tree, &[fork], "t1", &id).expect("t1");
        let merge_t1_tree =
            files_tree(&repo, &[("base", b"base\n"), ("t1.txt", b"t1\n")]).expect("merge t1 tree");
        let merge_t1 =
            write_commit(&repo, merge_t1_tree, &[fork, t1], "merge t1", &id).expect("merge t1");
        let t2_tree = blob_tree(&repo, "t2.txt", b"t2\n").expect("t2 tree");
        let t2 = write_commit(&repo, t2_tree, &[merge_t1], "t2", &id).expect("t2");
        let merge_t2_tree = files_tree(
            &repo,
            &[
                ("base", b"base\n"),
                ("t1.txt", b"t1\n"),
                ("t2.txt", b"t2\n"),
            ],
        )
        .expect("merge t2 tree");
        let merge_t2 =
            write_commit(&repo, merge_t2_tree, &[merge_t1, t2], "merge t2", &id).expect("merge t2");

        let range_set: HashSet<ObjectId> = [t1, merge_t1, t2, merge_t2].into_iter().collect();
        let mainline =
            mainline_commits_in_range(&repo, fork, &[merge_t2], &range_set).expect("mainline");
        assert!(mainline.contains(&merge_t1));
        assert!(mainline.contains(&merge_t2));
        assert!(!mainline.contains(&t1));
        assert!(!mainline.contains(&t2));

        let partitioned = vec![t1, t2, merge_t1, merge_t2];
        let ordered =
            priority_topo_replay_order(&repo, &mainline, &partitioned).expect("ordered replay");
        assert_eq!(ordered, vec![t1, merge_t1, t2, merge_t2]);
    }
}
