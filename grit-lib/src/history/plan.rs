//! In-memory history rewrite planning.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::change_id::{change_id, set_change_id, ChangeId, ChangeIdGenerator, ChangeIdPolicy};
use crate::commit::signing::extra_headers_without_commit_signatures;
use crate::commit_encoding::identity_raw_for_serialized_commit;
use crate::error::Error;
use crate::merge_base::is_ancestor;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{ReplayConflictLabels, TreeMergeOutput};
use crate::objects::{serialize_commit, CommitData, HashAlgo, ObjectId, ObjectKind};
use crate::porcelain::commit::prepare_commit_bytes;
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::state::resolve_head_for_repository;
use crate::worktree::{find_worktree_with_branch_checked_out, worktree_admin_dir};

use super::error::HistoryError;
use super::replay_merge_session::ReplayMergeSession;
use super::subject::{
    commit_message_subject_line, replay_conflict_labels, subject_for_rewrite_row,
};
use super::types::{
    CommitEdit, EmptyPolicy, HeadEffect, HistoryEdits, PlannedRefUpdate, RefScope, RewriteOptions,
    RewritePlan, RewrittenCommit,
};

// hygiene: resumable replay control (thread-local for parallel tests)
thread_local! {
    static RESUMABLE_REPLAY: Cell<bool> = const { Cell::new(false) };
    static REPLAY_STOP_PARENT: Cell<Option<ObjectId>> = const { Cell::new(None) };
    static REPLAY_STOP_COMMIT: Cell<Option<ObjectId>> = const { Cell::new(None) };
    static REPLAY_STOP_MERGE: RefCell<Option<TreeMergeOutput>> = const { RefCell::new(None) };
}

/// Conflict snapshot when resumable replay stops mid-plan.
pub(crate) struct PartialReplayConflict {
    /// Original commit being replayed when the tree merge conflicted.
    pub commit: ObjectId,
    /// Mapped first parent (detached `HEAD` target during stop/resume).
    pub replayed_parent: ObjectId,
    /// Three-way merge output with conflict stages and marker payloads.
    pub merge: TreeMergeOutput,
}

/// Partial progress returned when resumable planning stops on a conflict.
pub(crate) struct PartialRewritePlan {
    /// Old→new OIDs for commits fully replayed before the stop.
    pub mapping: HashMap<ObjectId, ObjectId>,
    /// Rewrite rows for commits handled before the stop.
    pub rewritten: Vec<RewrittenCommit>,
    /// Conflict at the stopping commit.
    pub conflict: PartialReplayConflict,
    /// Full replay order for the rewrite (including not-yet-replayed commits).
    pub commits_to_replay: Vec<ObjectId>,
    /// Scoped branch tips collected at plan start.
    pub scoped_refs: Vec<(String, ObjectId)>,
    /// Edits map used for this plan.
    pub edits: HistoryEdits,
    /// Rewrite options.
    pub opts: RewriteOptions,
    /// Start of the linear chain when planning linearizes the replay (see
    /// [`plan_rewrite_with_order`]); `None` when merges are replayed.
    pub linearize_onto: Option<ObjectId>,
}

/// Outcome of [`plan_rewrite_with_order_resumable`].
pub(crate) enum ResumablePlanOutcome {
    /// Every commit in the replay order was planned.
    Complete(Box<RewritePlan>),
    /// Replay stopped on a conflict; partial mapping and ODB objects are retained.
    Partial(Box<PartialRewritePlan>),
}

/// Run `f` with resumable conflict capture enabled for tree replay during planning.
pub(crate) fn with_resumable_replay<R>(f: impl FnOnce() -> R) -> R {
    RESUMABLE_REPLAY.with(|f| f.set(true));
    REPLAY_STOP_PARENT.with(|f| f.set(None));
    REPLAY_STOP_COMMIT.with(|f| f.set(None));
    REPLAY_STOP_MERGE.with(|f| *f.borrow_mut() = None);
    let out = f();
    RESUMABLE_REPLAY.with(|f| f.set(false));
    out
}

fn note_resumable_conflict(commit: ObjectId, replayed_parent: ObjectId, merge: TreeMergeOutput) {
    if !RESUMABLE_REPLAY.with(Cell::get) {
        return;
    }
    REPLAY_STOP_COMMIT.with(|c| c.set(Some(commit)));
    REPLAY_STOP_PARENT.with(|p| p.set(Some(replayed_parent)));
    REPLAY_STOP_MERGE.with(|m| *m.borrow_mut() = Some(merge));
}

fn take_resumable_conflict() -> Option<PartialReplayConflict> {
    let commit = REPLAY_STOP_COMMIT.with(|c| c.take())?;
    let replayed_parent = REPLAY_STOP_PARENT.with(|p| p.take())?;
    let merge = REPLAY_STOP_MERGE.with(|m| m.borrow_mut().take())?;
    Some(PartialReplayConflict {
        commit,
        replayed_parent,
        merge,
    })
}

struct ChangeIdWriteContext {
    policy: ChangeIdPolicy,
    generator: Arc<dyn ChangeIdGenerator>,
    hash_algo: HashAlgo,
}

impl ChangeIdWriteContext {
    fn from_repo(repo: &Repository) -> Result<Self, HistoryError> {
        let config = repo.config().map_err(library_err)?;
        Ok(Self {
            policy: ChangeIdPolicy::from_config(config.as_ref()),
            generator: repo.change_id_generator(),
            hash_algo: repo.odb().hash_algo(),
        })
    }
}

/// Build a rewrite plan: walk affected history, write replacement commits to the ODB, and
/// record ref movements without touching refs, the index, or the worktree.
///
/// Replacement commits preserve `change-id`, legacy `gitbutler-change-id`, and other extra
/// headers from the template (signatures are stripped). When [`ChangeIdPolicy`](crate::change_id::ChangeIdPolicy)
/// is enabled and the source commit has no stored change id, the rewritten commit stores the
/// Jujutsu-derived id for the original commit oid. New commits inserted by the plan (for example
/// split parents) receive fresh ids from [`ChangeIdPolicy::id_for_new_commit`](crate::change_id::ChangeIdPolicy::id_for_new_commit).
///
/// When [`RewriteOptions::sign`] is `true`, replacement commits honor `commit.gpgsign` via
/// [`prepare_commit_bytes`](crate::porcelain::commit::prepare_commit_bytes). When `sign` is
/// `false`, commits are written unsigned and external signers are not invoked.
///
/// # Errors
///
/// Returns [`HistoryError`] for conflicts, unsupported merges, scope violations, empty-policy
/// aborts, [`HistoryError::Planning`] for invalid planning inputs, and [`HistoryError::Library`]
/// for object/revwalk/ref failures.
pub fn plan_rewrite(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    plan_rewrite_with_order(repo, edits, opts, None, None, MergeFavor::default())
}

/// Turn a replay conflict during resumable planning into partial progress; other errors pass through.
#[allow(clippy::too_many_arguments)]
fn finish_plan_rewrite_on_replay_error(
    repo: &Repository,
    err: HistoryError,
    mapping: HashMap<ObjectId, ObjectId>,
    rewritten: Vec<RewrittenCommit>,
    commits_to_replay: Vec<ObjectId>,
    scoped_refs: Vec<(String, ObjectId)>,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
    linearize_onto: Option<ObjectId>,
) -> Result<ResumablePlanOutcome, HistoryError> {
    if matches!(err, HistoryError::Conflict { .. }) {
        if let Some(conflict) = take_resumable_conflict() {
            return Ok(ResumablePlanOutcome::Partial(Box::new(
                PartialRewritePlan {
                    mapping,
                    rewritten,
                    conflict,
                    commits_to_replay,
                    scoped_refs,
                    edits: edits.clone(),
                    opts: opts.clone(),
                    linearize_onto,
                },
            )));
        }
    }
    Err(attribute_conflict_to_refs(repo, err, &scoped_refs))
}

/// Like [`plan_rewrite_with_order`], but may stop on the first replay conflict and return partial progress.
pub(crate) fn plan_rewrite_with_order_resumable(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
    replay_order: Option<&[ObjectId]>,
    linearize_onto: Option<ObjectId>,
    merge_favor: MergeFavor,
) -> Result<ResumablePlanOutcome, HistoryError> {
    plan_rewrite_with_order_resumable_seeded(
        repo,
        edits,
        opts,
        replay_order,
        linearize_onto,
        merge_favor,
        None,
        None,
    )
}

/// Resumable planning with an existing partial mapping (continue/skip after a stop).
#[allow(clippy::too_many_arguments)]
pub(crate) fn plan_rewrite_with_order_resumable_seeded(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
    replay_order: Option<&[ObjectId]>,
    linearize_onto: Option<ObjectId>,
    merge_favor: MergeFavor,
    seed: Option<(HashMap<ObjectId, ObjectId>, Vec<RewrittenCommit>)>,
    scoped_refs: Option<Vec<(String, ObjectId)>>,
) -> Result<ResumablePlanOutcome, HistoryError> {
    with_resumable_replay(|| {
        plan_rewrite_with_order_impl(
            repo,
            edits,
            opts,
            replay_order,
            linearize_onto,
            merge_favor,
            seed,
            scoped_refs,
        )
    })
}

/// Like [`plan_rewrite`], but when `replay_order` is set, replays commits in that exact
/// topological order (used by [`super::rebase::rebase_onto`](crate::history::rebase::rebase_onto)
/// so merge side branches are included).
///
/// `linearize_onto` makes the replay one line starting at that commit, the way `git rebase`
/// without `--rebase-merges` replays a branch: a [`CommitEdit::Drop`] or [`CommitEdit::Reuse`]
/// entry maps to the tip of the line built so far instead of its own parent, so dropped merges
/// and patches already upstream never pull the branch back off the line. The caller chains the replayed commits with [`CommitEdit::Reparent`] edits.
pub(crate) fn plan_rewrite_with_order(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
    replay_order: Option<&[ObjectId]>,
    linearize_onto: Option<ObjectId>,
    merge_favor: MergeFavor,
) -> Result<RewritePlan, HistoryError> {
    match plan_rewrite_with_order_impl(
        repo,
        edits,
        opts,
        replay_order,
        linearize_onto,
        merge_favor,
        None,
        None,
    )? {
        ResumablePlanOutcome::Complete(plan) => Ok(*plan),
        ResumablePlanOutcome::Partial(_) => unreachable!("non-resumable planning cannot stop"),
    }
}

#[allow(clippy::too_many_arguments)]
fn plan_rewrite_with_order_impl(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
    replay_order: Option<&[ObjectId]>,
    linearize_onto: Option<ObjectId>,
    merge_favor: MergeFavor,
    seed: Option<(HashMap<ObjectId, ObjectId>, Vec<RewrittenCommit>)>,
    scoped_refs_override: Option<Vec<(String, ObjectId)>>,
) -> Result<ResumablePlanOutcome, HistoryError> {
    let committer_line = opts.committer_line()?;
    if edits.is_empty() {
        return Ok(ResumablePlanOutcome::Complete(Box::new(RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        })));
    }

    let change_ids = ChangeIdWriteContext::from_repo(repo)?;

    let oldest = oldest_edited_commit(repo, edits)?;
    let scoped_refs = if let Some(refs) = scoped_refs_override {
        refs
    } else {
        collect_scoped_refs(repo, opts, oldest)?
    };
    ensure_branches_not_checked_out_elsewhere(repo, &scoped_refs)?;

    let commits_to_replay = if let Some(order) = replay_order {
        let order = order.to_vec();
        for &edited in edits.keys() {
            if !order.contains(&edited) {
                return Err(HistoryError::NotInHistory { commit: edited });
            }
        }
        order
    } else {
        let parents_of_oldest = commit_parents(repo, oldest)?;
        let tips: Vec<String> = scoped_refs
            .iter()
            .map(|(_, oid)| oid.to_hex())
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();

        let negative: Vec<String> = parents_of_oldest.iter().map(ObjectId::to_hex).collect();

        let walk_opts = RevListOptions {
            ancestry_path: true,
            ancestry_path_bottoms: vec![oldest],
            ordering: OrderingMode::Topo,
            reverse: true,
            use_commit_graph: true,
            full_history: true,
            ..RevListOptions::default()
        };

        let walk = rev_list(repo, &tips, &negative, &walk_opts).map_err(library_err)?;
        let mut commits_to_replay: Vec<ObjectId> = walk.commits;
        if !commits_to_replay.contains(&oldest) {
            commits_to_replay.insert(0, oldest);
        }

        for &edited in edits.keys() {
            if !commits_to_replay.contains(&edited) {
                return Err(HistoryError::NotInHistory { commit: edited });
            }
        }
        commits_to_replay
    };

    if matches!(opts.scope, RefScope::Head) && seed.is_none() {
        let head_oid = repo.head_commit()?;
        for &edited in edits.keys() {
            if !is_ancestor(repo, edited, head_oid).map_err(library_err)? {
                return Err(HistoryError::NotInHistory { commit: edited });
            }
        }
    }

    if replay_order.is_none() {
        for (name, tip) in &scoped_refs {
            if is_ancestor(repo, oldest, *tip).map_err(library_err)?
                && !commits_to_replay.contains(tip)
            {
                return Err(HistoryError::DivergentBranch { name: name.clone() });
            }
        }
    }

    let (mut mapping, mut rewritten) = if let Some((mapping, rewritten)) = seed {
        (mapping, rewritten)
    } else {
        (HashMap::new(), Vec::with_capacity(commits_to_replay.len()))
    };
    let skip_replayed: HashSet<ObjectId> = mapping.keys().copied().collect();
    // Tip of the linear chain built so far when linearizing; every replay-order entry maps to
    // the tip as it stood once that entry was handled.
    let mut linear_tip = linearize_onto;
    let mut replay_session = ReplayMergeSession::default();

    for &old_oid in &commits_to_replay {
        if skip_replayed.contains(&old_oid) {
            if linear_tip.is_some() {
                linear_tip = mapping.get(&old_oid).copied().or(linear_tip);
            }
            continue;
        }
        let old = load_commit(repo, old_oid)?;
        let edit = edits.get(&old_oid);

        if matches!(
            edit,
            Some(CommitEdit::InsertBefore(_)) | Some(CommitEdit::SplitBefore { .. })
        ) && old.parents.len() > 1
        {
            return Err(HistoryError::InsertBeforeOnMerge { commit: old_oid });
        }

        if let Some(CommitEdit::Reuse { existing, parent }) = edit {
            // Descendants and refs continue from the replay chain, while the row still names
            // the upstream commit. When the reused commit is the source itself, keep that OID
            // in the mapping so refs and `HEAD` stay put (identity `--exec` replay).
            if *existing == old_oid {
                mapping.insert(old_oid, *existing);
                if linear_tip.is_some() {
                    linear_tip = Some(*existing);
                }
            } else {
                let continuation =
                    linear_tip.unwrap_or_else(|| mapping.get(parent).copied().unwrap_or(*parent));
                mapping.insert(old_oid, continuation);
            }
            rewritten.push(RewrittenCommit {
                old: old_oid,
                new: Some(*existing),
                change_id: logical_change_id_for_commit(repo, *existing)?,
                subject: subject_for_rewrite_row(repo, &old, Some(*existing))?,
                reused: true,
            });
            continue;
        }

        if matches!(edit, Some(CommitEdit::Drop)) {
            if old.parents.is_empty() {
                return Err(HistoryError::RootCommit { commit: old_oid });
            }
            let parent = match linear_tip {
                Some(tip) => tip,
                None => mapped_first_parent(&mapping, &old, old_oid)?,
            };
            mapping.insert(old_oid, parent);
            rewritten.push(RewrittenCommit {
                old: old_oid,
                new: None,
                change_id: None,
                subject: subject_for_rewrite_row(repo, &old, None)?,
                reused: false,
            });
            continue;
        }

        let insert_parent = match edit {
            Some(CommitEdit::InsertBefore(insert))
            | Some(CommitEdit::SplitBefore { insert, .. }) => {
                let parents = if old.parents.is_empty() {
                    Vec::new()
                } else {
                    vec![mapped_first_parent(&mapping, &old, old_oid)?]
                };
                Some(write_new_commit(
                    repo,
                    opts,
                    &change_ids,
                    insert.tree,
                    &parents,
                    &insert.author,
                    &committer_line,
                    &insert.message,
                    &[],
                )?)
            }
            _ => None,
        };

        if RESUMABLE_REPLAY.with(Cell::get) {
            if let Ok(replayed_parent) =
                replay_stop_parent(edit, &old, old_oid, insert_parent, &mapping)
            {
                REPLAY_STOP_PARENT.with(|p| p.set(Some(replayed_parent)));
                REPLAY_STOP_COMMIT.with(|c| c.set(Some(old_oid)));
            }
        }

        let mut replay_step = || -> Result<(ObjectId, Option<ObjectId>), HistoryError> {
            match edit {
                None => {
                    let oid = replay_commit_descendant(
                        repo,
                        &change_ids,
                        &mapping,
                        &old,
                        old_oid,
                        insert_parent,
                        merge_favor,
                        opts,
                        &mut replay_session,
                    )?;
                    Ok((oid, Some(oid)))
                }
                Some(CommitEdit::InsertBefore(_)) => {
                    let oid = replay_commit_descendant(
                        repo,
                        &change_ids,
                        &mapping,
                        &old,
                        old_oid,
                        insert_parent,
                        merge_favor,
                        opts,
                        &mut replay_session,
                    )?;
                    Ok((oid, Some(oid)))
                }
                Some(CommitEdit::SplitBefore { rest_message, .. }) => {
                    let parent_list = if let Some(inserted) = insert_parent {
                        vec![inserted]
                    } else if old.parents.is_empty() {
                        Vec::new()
                    } else {
                        vec![mapped_first_parent(&mapping, &old, old_oid)?]
                    };
                    let message = rest_message.as_deref().unwrap_or(&old.message);
                    let committer = if opts.preserve_committer {
                        old.committer.as_str()
                    } else {
                        committer_line.as_str()
                    };
                    let oid = write_replacement_commit(
                        repo,
                        opts,
                        &change_ids,
                        &old,
                        old_oid,
                        old.tree,
                        &parent_list,
                        &old.author,
                        committer,
                        message,
                        true,
                    )?;
                    Ok((oid, Some(oid)))
                }
                Some(CommitEdit::Reparent { parent }) => {
                    let parent_list = reparent_parent_list(&old, *parent, &mapping);
                    let oid = if old.parents.len() > 1 {
                        replay_merge_descendant(
                            repo,
                            &change_ids,
                            &mapping,
                            &old,
                            old_oid,
                            None,
                            merge_favor,
                            opts,
                            Some(&parent_list),
                            &mut replay_session,
                        )?
                    } else {
                        replay_edited_commit(
                            repo,
                            &change_ids,
                            &old,
                            old_oid,
                            &parent_list,
                            &old.author,
                            &committer_line,
                            &old.message,
                            merge_favor,
                            opts,
                            &mut replay_session,
                        )?
                    };
                    Ok((oid, Some(oid)))
                }
                Some(CommitEdit::Rewrite {
                    tree,
                    message,
                    author,
                    parents,
                }) => {
                    let parent_list = if let Some(parents) = parents {
                        parents.clone()
                    } else if let Some(inserted) = insert_parent {
                        vec![inserted]
                    } else if old.parents.is_empty() {
                        Vec::new()
                    } else if old.parents.len() > 1 {
                        old.parents
                            .iter()
                            .map(|p| mapping.get(p).copied().unwrap_or(*p))
                            .collect()
                    } else {
                        vec![mapped_first_parent(&mapping, &old, old_oid)?]
                    };
                    let author = author.as_deref().unwrap_or(&old.author);
                    let message = message.as_deref().unwrap_or(&old.message);
                    let committer = if opts.preserve_committer {
                        old.committer.as_str()
                    } else {
                        committer_line.as_str()
                    };
                    let oid = if let Some(explicit_tree) = *tree {
                        write_replacement_commit(
                            repo,
                            opts,
                            &change_ids,
                            &old,
                            old_oid,
                            explicit_tree,
                            &parent_list,
                            author,
                            committer,
                            message,
                            false,
                        )?
                    } else {
                        replay_edited_commit(
                            repo,
                            &change_ids,
                            &old,
                            old_oid,
                            &parent_list,
                            author,
                            committer,
                            message,
                            merge_favor,
                            opts,
                            &mut replay_session,
                        )?
                    };
                    Ok((oid, Some(oid)))
                }
                Some(CommitEdit::Drop) | Some(CommitEdit::Reuse { .. }) => {
                    unreachable!("handled above")
                }
            }
        };

        let (new_oid, new_field) = match replay_step() {
            Ok(v) => v,
            Err(err) => {
                return finish_plan_rewrite_on_replay_error(
                    repo,
                    err,
                    mapping,
                    rewritten,
                    commits_to_replay.clone(),
                    scoped_refs.clone(),
                    edits,
                    opts,
                    linearize_onto,
                );
            }
        };

        mapping.insert(old_oid, new_oid);
        if linear_tip.is_some() {
            linear_tip = Some(new_oid);
        }
        rewritten.push(RewrittenCommit {
            old: old_oid,
            new: new_field,
            change_id: match new_field {
                Some(oid) => logical_change_id_for_commit(repo, oid)?,
                None => None,
            },
            subject: subject_for_rewrite_row(repo, &old, new_field)?,
            reused: false,
        });
    }

    let ref_updates = plan_ref_updates(&scoped_refs, &mapping);
    let head = head_effect(repo, &mapping)?;

    Ok(ResumablePlanOutcome::Complete(Box::new(RewritePlan {
        commits: rewritten,
        ref_updates,
        head,
    })))
}

/// Replay a contiguous first-parent segment in a custom order (each commit onto the previous).
///
/// `oldest` must be the first commit in the segment on the original first-parent chain;
/// `replay_order` lists the same commits in the desired new order.
///
/// # Errors
///
/// Same as [`plan_rewrite`], including [`HistoryError::MergeCommitInReplay`] when the segment
/// contains a merge commit.
pub fn plan_replay_linear_segment(
    repo: &Repository,
    oldest: ObjectId,
    replay_order: &[ObjectId],
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    let committer_line = opts.committer_line()?;
    if replay_order.is_empty() {
        return Ok(RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        });
    }

    let change_ids = ChangeIdWriteContext::from_repo(repo)?;

    let scoped_refs = collect_scoped_refs(repo, opts, oldest)?;
    ensure_branches_not_checked_out_elsewhere(repo, &scoped_refs)?;

    let parents_of_oldest = commit_parents(repo, oldest)?;
    let (commits_to_replay, segment_tip_old) =
        validate_replay_order_segment(repo, oldest, replay_order)?;
    let segment_set: HashSet<ObjectId> = commits_to_replay.iter().copied().collect();

    if matches!(opts.scope, RefScope::Head) {
        let head_oid = repo.head_commit()?;
        if !is_ancestor(repo, oldest, head_oid).map_err(library_err)? {
            return Err(HistoryError::NotInHistory { commit: oldest });
        }
    }

    if let Some(main_segment) = first_parent_segment_between(repo, oldest, segment_tip_old)? {
        let main_line: HashSet<ObjectId> = main_segment.iter().copied().collect();
        for (name, tip) in &scoped_refs {
            if segment_set.contains(tip) {
                continue;
            }
            if main_line.contains(tip) {
                return Err(HistoryError::DivergentBranch { name: name.clone() });
            }
        }
    }

    for &oid in &commits_to_replay {
        let old = load_commit(repo, oid)?;
        if old.parents.len() > 1 {
            return Err(HistoryError::MergeCommitInReplay { commit: oid });
        }
    }

    let mut mapping: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut parent: Option<ObjectId> = parents_of_oldest.first().copied();
    let mut replay_session = ReplayMergeSession::default();

    for &old_oid in replay_order {
        let old = load_commit(repo, old_oid)?;
        let parent_list: Vec<ObjectId> = parent.into_iter().collect();
        let new_oid = replay_edited_commit(
            repo,
            &change_ids,
            &old,
            old_oid,
            &parent_list,
            &old.author,
            &committer_line,
            &old.message,
            MergeFavor::default(),
            opts,
            &mut replay_session,
        )?;
        mapping.insert(old_oid, new_oid);
        parent = Some(new_oid);
    }

    let rewritten = commits_to_replay
        .iter()
        .map(|&old| {
            let new = mapping.get(&old).copied().unwrap_or(old);
            let old_commit = load_commit(repo, old)?;
            Ok(RewrittenCommit {
                old,
                new: Some(new),
                change_id: logical_change_id_for_commit(repo, new)?,
                subject: subject_for_rewrite_row(repo, &old_commit, Some(new))?,
                reused: false,
            })
        })
        .collect::<Result<Vec<_>, HistoryError>>()?;

    let replay_tip = *replay_order.last().ok_or(HistoryError::Planning {
        message: "empty replay order".into(),
    })?;
    let segment_tip_new = mapping
        .get(&replay_tip)
        .copied()
        .ok_or(HistoryError::Planning {
            message: "replay tip missing from mapping".into(),
        })?;
    let ref_updates =
        plan_ref_updates_reordered(&scoped_refs, &mapping, segment_tip_old, segment_tip_new);
    let mut head_mapping = mapping.clone();
    if let Ok(head_oid) = repo.head_commit() {
        if head_oid == segment_tip_old {
            head_mapping.insert(segment_tip_old, segment_tip_new);
        }
    }
    let head = head_effect(repo, &head_mapping)?;

    Ok(RewritePlan {
        commits: rewritten,
        ref_updates,
        head,
    })
}

fn validate_replay_order_segment(
    repo: &Repository,
    oldest: ObjectId,
    replay_order: &[ObjectId],
) -> Result<(Vec<ObjectId>, ObjectId), HistoryError> {
    if replay_order.is_empty() {
        return Err(HistoryError::Planning {
            message: "replay order must not be empty".into(),
        });
    }
    let set: HashSet<ObjectId> = replay_order.iter().copied().collect();
    if set.len() != replay_order.len() {
        return Err(HistoryError::Planning {
            message: "replay order must not list the same commit twice".into(),
        });
    }
    let segment_tip = segment_tip_in_replay_order(repo, replay_order)?;
    let Some(canonical) = first_parent_segment_between(repo, oldest, segment_tip)? else {
        return Err(HistoryError::Planning {
            message: "replay order is not a first-parent segment from oldest".into(),
        });
    };
    if canonical.first() != Some(&oldest) {
        return Err(HistoryError::Planning {
            message: "oldest does not start the replay segment".into(),
        });
    }
    if canonical.iter().copied().collect::<HashSet<_>>() != set {
        return Err(HistoryError::Planning {
            message: "replay order must cover exactly one first-parent segment".into(),
        });
    }
    Ok((canonical, segment_tip))
}

fn segment_tip_in_replay_order(
    repo: &Repository,
    replay_order: &[ObjectId],
) -> Result<ObjectId, HistoryError> {
    let set: HashSet<ObjectId> = replay_order.iter().copied().collect();
    let mut tips = Vec::new();
    for &oid in replay_order {
        let is_parent_of_other = replay_order.iter().any(|&child| {
            child != oid
                && commit_parents(repo, child)
                    .ok()
                    .and_then(|parents| parents.first().copied())
                    == Some(oid)
        });
        if !is_parent_of_other {
            tips.push(oid);
        }
    }
    if tips.len() != 1 {
        return Err(HistoryError::Planning {
            message: "replay order must have exactly one segment tip".into(),
        });
    }
    let tip = tips[0];
    if !set.contains(&tip) {
        return Err(HistoryError::Planning {
            message: "replay segment tip missing from replay order".into(),
        });
    }
    Ok(tip)
}

/// First-parent commits from `oldest` through `tip`, inclusive; `None` when `tip` is not reachable.
fn first_parent_segment_between(
    repo: &Repository,
    oldest: ObjectId,
    tip: ObjectId,
) -> Result<Option<Vec<ObjectId>>, HistoryError> {
    let mut newest_first = Vec::new();
    let mut current = Some(tip);
    while let Some(oid) = current {
        newest_first.push(oid);
        if oid == oldest {
            newest_first.reverse();
            return Ok(Some(newest_first));
        }
        let parents = commit_parents(repo, oid)?;
        current = parents.first().copied();
    }
    Ok(None)
}

fn plan_ref_updates_reordered(
    scoped_refs: &[(String, ObjectId)],
    mapping: &HashMap<ObjectId, ObjectId>,
    segment_tip_old: ObjectId,
    segment_tip_new: ObjectId,
) -> Vec<PlannedRefUpdate> {
    let mut updates = Vec::new();
    for (name, old_tip) in scoped_refs {
        let new_tip = if *old_tip == segment_tip_old {
            segment_tip_new
        } else {
            resolve_mapped_tip(*old_tip, mapping)
        };
        if new_tip != *old_tip {
            updates.push(PlannedRefUpdate {
                name: name.clone(),
                old: *old_tip,
                new: new_tip,
            });
        }
    }
    updates.sort_by(|a, b| a.name.cmp(&b.name));
    updates
}

fn library_err(err: Error) -> HistoryError {
    if let Error::History(inner) = err {
        return inner;
    }
    if matches!(err, Error::ObjectNotFound(_)) {
        return HistoryError::Library(Box::new(err));
    }
    HistoryError::Planning {
        message: err.to_string(),
    }
}

fn oldest_edited_commit(repo: &Repository, edits: &HistoryEdits) -> Result<ObjectId, HistoryError> {
    let keys: Vec<ObjectId> = edits.keys().copied().collect();
    for &candidate in &keys {
        let mut is_oldest = true;
        for &other in &keys {
            if other == candidate {
                continue;
            }
            if is_ancestor(repo, other, candidate).map_err(library_err)? {
                is_oldest = false;
                break;
            }
        }
        if is_oldest {
            return Ok(candidate);
        }
    }
    let fallback = edits
        .keys()
        .next()
        .copied()
        .unwrap_or_else(|| ObjectId::null(repo.odb().hash_algo()));
    Err(HistoryError::NotInHistory { commit: fallback })
}

pub(crate) fn collect_scoped_refs(
    repo: &Repository,
    opts: &RewriteOptions,
    oldest: ObjectId,
) -> Result<Vec<(String, ObjectId)>, HistoryError> {
    let head = resolve_head_for_repository(repo).map_err(library_err)?;
    let mut out = Vec::new();

    match opts.scope {
        RefScope::Head => match head {
            crate::state::HeadState::Branch { refname, oid, .. } => {
                let Some(oid) = oid else {
                    return Err(HistoryError::NotInHistory { commit: oldest });
                };
                if !is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    return Err(HistoryError::NotInHistory { commit: oldest });
                }
                out.push((refname, oid));
            }
            crate::state::HeadState::Detached { oid } => {
                if !is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    return Err(HistoryError::NotInHistory { commit: oldest });
                }
                out.push(("HEAD".to_owned(), oid));
            }
            crate::state::HeadState::Invalid => {
                return Err(HistoryError::NotInHistory { commit: oldest });
            }
        },
        RefScope::Branches => {
            let (checked_out, head_tip) = match &head {
                crate::state::HeadState::Branch { refname, oid, .. } => {
                    (Some(refname.as_str()), *oid)
                }
                crate::state::HeadState::Detached { oid } => (None, Some(*oid)),
                crate::state::HeadState::Invalid => (None, None),
            };
            let branches = repo.references().list("refs/heads/").map_err(library_err)?;
            for (name, oid) in branches {
                // Another branch at the checked-out tip is a backup of it, not a stacked
                // branch, so it keeps the pre-rewrite commits.
                if head_tip == Some(oid) && checked_out != Some(name.as_str()) {
                    continue;
                }
                if is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    out.push((name, oid));
                }
            }
            if let crate::state::HeadState::Detached { oid } = head {
                if is_ancestor(repo, oldest, oid).map_err(library_err)? {
                    out.push(("HEAD".to_owned(), oid));
                }
            }
        }
    }

    if out.is_empty() {
        return Err(HistoryError::NotInHistory { commit: oldest });
    }
    Ok(out)
}

fn ensure_branches_not_checked_out_elsewhere(
    repo: &Repository,
    scoped_refs: &[(String, ObjectId)],
) -> Result<(), HistoryError> {
    let here = worktree_admin_dir(repo);
    for (name, _) in scoped_refs {
        let short = name.strip_prefix("refs/heads/").unwrap_or(name.as_str());
        if short == "HEAD" {
            continue;
        }
        if let Some(found) =
            find_worktree_with_branch_checked_out(repo, short, Some(&here)).map_err(library_err)?
        {
            return Err(HistoryError::BranchCheckedOutElsewhere {
                name: name.clone(),
                worktree: crate::git_path::path_for_error_message(&found.worktree_path),
            });
        }
    }
    Ok(())
}

pub(crate) fn commit_parents(
    repo: &Repository,
    oid: ObjectId,
) -> Result<Vec<ObjectId>, HistoryError> {
    Ok(load_commit(repo, oid)?.parents)
}

fn load_commit(repo: &Repository, oid: ObjectId) -> Result<CommitData, HistoryError> {
    repo.odb().read_commit(&oid).map_err(library_err)
}

fn mapped_first_parent(
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    commit: ObjectId,
) -> Result<ObjectId, HistoryError> {
    let parent = *old
        .parents
        .first()
        .ok_or(HistoryError::RootCommit { commit })?;
    Ok(mapping.get(&parent).copied().unwrap_or(parent))
}

#[allow(clippy::too_many_arguments)]
fn replay_commit_descendant(
    repo: &Repository,
    change_ids: &ChangeIdWriteContext,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    old_oid: ObjectId,
    extra_parent: Option<ObjectId>,
    merge_favor: MergeFavor,
    opts: &RewriteOptions,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    if old.parents.len() > 1 {
        return replay_merge_descendant(
            repo,
            change_ids,
            mapping,
            old,
            old_oid,
            extra_parent,
            merge_favor,
            opts,
            None,
            replay_session,
        );
    }
    replay_descendant(
        repo,
        change_ids,
        mapping,
        old,
        old_oid,
        extra_parent,
        merge_favor,
        opts,
        replay_session,
    )
}

/// Replay `commit` onto the replacements `mapping` records for its parents, keeping its
/// author, message and change id.
///
/// Returns the replacement, or the mapped first parent when the replay became empty and
/// `opts.empty` is [`EmptyPolicy::Drop`].
///
/// # Errors
///
/// [`HistoryError::Conflict`] when the tree merge conflicts, [`HistoryError::BecameEmpty`]
/// under [`EmptyPolicy::Abort`], and object database or signing failures.
pub(crate) fn replay_commit_onto_mapping(
    repo: &Repository,
    mapping: &HashMap<ObjectId, ObjectId>,
    commit: ObjectId,
    opts: &RewriteOptions,
) -> Result<ObjectId, HistoryError> {
    let change_ids = ChangeIdWriteContext::from_repo(repo)?;
    let old = load_commit(repo, commit)?;
    let mut replay_session = ReplayMergeSession::default();
    replay_commit_descendant(
        repo,
        &change_ids,
        mapping,
        &old,
        commit,
        None,
        MergeFavor::default(),
        opts,
        &mut replay_session,
    )
}

/// Write the replacement of `template` with a caller-resolved `tree` on top of `parent`,
/// keeping its author, message and change id.
///
/// # Errors
///
/// Object database or signing failures.
pub(crate) fn write_resolved_replay(
    repo: &Repository,
    template: ObjectId,
    tree: ObjectId,
    parent: ObjectId,
    opts: &RewriteOptions,
) -> Result<ObjectId, HistoryError> {
    let change_ids = ChangeIdWriteContext::from_repo(repo)?;
    let old = load_commit(repo, template)?;
    let committer = if opts.preserve_committer {
        old.committer.clone()
    } else {
        opts.committer_line()?
    };
    write_rewritten_commit(
        repo,
        opts,
        &change_ids,
        &old,
        template,
        tree,
        &[parent],
        &old.author,
        &committer,
        &old.message,
    )
}

#[allow(clippy::too_many_arguments)]
fn replay_descendant(
    repo: &Repository,
    change_ids: &ChangeIdWriteContext,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    old_oid: ObjectId,
    extra_parent: Option<ObjectId>,
    merge_favor: MergeFavor,
    opts: &RewriteOptions,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    let committer_line = opts.committer_line()?;
    let (_, new_parent) = replay_parent_pair(repo, old, mapping, extra_parent)?;
    replay_edited_commit(
        repo,
        change_ids,
        old,
        old_oid,
        &[new_parent],
        &old.author,
        &committer_line,
        &old.message,
        merge_favor,
        opts,
        replay_session,
    )
}

#[allow(clippy::too_many_arguments)]
fn replay_merge_descendant(
    repo: &Repository,
    change_ids: &ChangeIdWriteContext,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    old_oid: ObjectId,
    extra_parent: Option<ObjectId>,
    merge_favor: MergeFavor,
    opts: &RewriteOptions,
    parent_list_override: Option<&[ObjectId]>,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    let committer_line = opts.committer_line()?;
    let parent_list = parent_list_override
        .map(<[_]>::to_vec)
        .unwrap_or_else(|| mapped_merge_parents(old, mapping, extra_parent));
    let result_tree = replay_merge_tree(
        repo,
        old,
        old_oid,
        &parent_list,
        merge_favor,
        replay_session,
    )?;

    let committer = if opts.preserve_committer {
        old.committer.as_str()
    } else {
        committer_line.as_str()
    };

    let empty = empty_tree_oid(repo)?;
    let old_first_parent = old.parents.first().copied().unwrap_or(empty);
    let new_first_parent = parent_list.first().copied().unwrap_or(empty);
    let old_parent_tree = if old.parents.is_empty() {
        empty
    } else {
        commit_tree(repo, old_first_parent)?
    };
    let new_parent_tree = if parent_list.is_empty() {
        empty
    } else {
        commit_tree(repo, new_first_parent)?
    };

    if old.tree != old_parent_tree && result_tree == new_parent_tree {
        return match opts.empty {
            EmptyPolicy::Abort => Err(HistoryError::BecameEmpty { commit: old_oid }),
            EmptyPolicy::Drop => Ok(new_first_parent),
            EmptyPolicy::Keep => write_rewritten_commit(
                repo,
                opts,
                change_ids,
                old,
                old_oid,
                result_tree,
                &parent_list,
                &old.author,
                committer,
                &old.message,
            ),
        };
    }

    write_rewritten_commit(
        repo,
        opts,
        change_ids,
        old,
        old_oid,
        result_tree,
        &parent_list,
        &old.author,
        committer,
        &old.message,
    )
}

fn mapped_merge_parents(
    old: &CommitData,
    mapping: &HashMap<ObjectId, ObjectId>,
    extra_first_parent: Option<ObjectId>,
) -> Vec<ObjectId> {
    old.parents
        .iter()
        .enumerate()
        .map(|(i, parent)| {
            if i == 0 {
                extra_first_parent
                    .unwrap_or_else(|| mapping.get(parent).copied().unwrap_or(*parent))
            } else {
                mapping.get(parent).copied().unwrap_or(*parent)
            }
        })
        .collect()
}

fn replay_stop_parent(
    edit: Option<&CommitEdit>,
    old: &CommitData,
    old_oid: ObjectId,
    insert_parent: Option<ObjectId>,
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Result<ObjectId, HistoryError> {
    let parent_list = match edit {
        Some(CommitEdit::Reparent { parent }) => reparent_parent_list(old, *parent, mapping),
        Some(CommitEdit::Rewrite {
            tree: _,
            message: _,
            author: _,
            parents,
        }) => {
            if let Some(parents) = parents {
                parents.clone()
            } else if let Some(inserted) = insert_parent {
                vec![inserted]
            } else if old.parents.is_empty() {
                Vec::new()
            } else if old.parents.len() > 1 {
                old.parents
                    .iter()
                    .map(|p| mapping.get(p).copied().unwrap_or(*p))
                    .collect()
            } else {
                vec![mapped_first_parent(mapping, old, old_oid)?]
            }
        }
        Some(CommitEdit::Drop) | Some(CommitEdit::Reuse { .. }) => {
            unreachable!("handled above")
        }
        Some(CommitEdit::InsertBefore(_)) | Some(CommitEdit::SplitBefore { .. }) | None => {
            if let Some(inserted) = insert_parent {
                vec![inserted]
            } else if old.parents.is_empty() {
                Vec::new()
            } else {
                vec![mapped_first_parent(mapping, old, old_oid)?]
            }
        }
    };
    parent_list
        .first()
        .copied()
        .ok_or(HistoryError::RootCommit { commit: old_oid })
}

fn reparent_parent_list(
    old: &CommitData,
    new_first_parent: ObjectId,
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Vec<ObjectId> {
    if old.parents.len() <= 1 {
        return vec![mapping
            .get(&new_first_parent)
            .copied()
            .unwrap_or(new_first_parent)];
    }
    std::iter::once(
        mapping
            .get(&new_first_parent)
            .copied()
            .unwrap_or(new_first_parent),
    )
    .chain(
        old.parents[1..]
            .iter()
            .map(|parent| mapping.get(parent).copied().unwrap_or(*parent)),
    )
    .collect()
}

/// Rebuild a merge commit tree after parent OIDs move, preserving merge-only changes and resolutions.
fn replay_merge_tree(
    repo: &Repository,
    old: &CommitData,
    old_oid: ObjectId,
    new_parents: &[ObjectId],
    merge_favor: MergeFavor,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    if old.parents.len() != new_parents.len() {
        return Err(HistoryError::Planning {
            message: "merge replay parent count mismatch".into(),
        });
    }

    let labels = replay_conflict_labels(old_oid, old);
    let mut result_tree = old.tree;
    for (&old_parent, &new_parent) in old.parents.iter().zip(new_parents.iter()) {
        if old_parent == new_parent {
            continue;
        }
        let old_parent_tree = commit_tree(repo, old_parent)?;
        let new_parent_tree = commit_tree(repo, new_parent)?;
        result_tree = transplant_tree_across_parent_move(
            repo,
            old_oid,
            &labels,
            result_tree,
            old_parent_tree,
            new_parent_tree,
            merge_favor,
            replay_session,
        )?;
    }
    Ok(result_tree)
}

/// Write the commit for a staged conflict resolution during [`super::rebase_session::continue_rebase`].
pub(crate) fn write_rebase_staged_resolution(
    repo: &Repository,
    opts: &RewriteOptions,
    stopped_commit: ObjectId,
    tree: ObjectId,
    parents: &[ObjectId],
) -> Result<ObjectId, HistoryError> {
    let old = load_commit(repo, stopped_commit)?;
    let change_ids = ChangeIdWriteContext::from_repo(repo)?;
    let committer = opts.committer_line()?;
    write_replacement_commit(
        repo,
        opts,
        &change_ids,
        &old,
        stopped_commit,
        tree,
        parents,
        &old.author,
        &committer,
        &old.message,
        false,
    )
}

/// Write a replacement commit for `old_oid`, honoring [`RewriteOptions::empty`].
///
/// When `split_rest_commit` is true (history split rest commit after an insert parent), the
/// result tree may match the insert parent's tree but the commit must still be written.
#[allow(clippy::too_many_arguments)]
fn write_replacement_commit(
    repo: &Repository,
    opts: &RewriteOptions,
    change_ids: &ChangeIdWriteContext,
    old: &CommitData,
    old_oid: ObjectId,
    result_tree: ObjectId,
    parent_list: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
    split_rest_commit: bool,
) -> Result<ObjectId, HistoryError> {
    let empty = empty_tree_oid(repo)?;
    let old_parent = old.parents.first().copied().unwrap_or(empty);
    let new_parent = parent_list.first().copied().unwrap_or(empty);
    let old_parent_tree = if old.parents.is_empty() {
        empty
    } else {
        commit_tree(repo, old_parent)?
    };
    let new_parent_tree = if parent_list.is_empty() {
        empty
    } else {
        commit_tree(repo, new_parent)?
    };

    if !split_rest_commit && old.tree != old_parent_tree && result_tree == new_parent_tree {
        return match opts.empty {
            EmptyPolicy::Abort => Err(HistoryError::BecameEmpty { commit: old_oid }),
            EmptyPolicy::Drop => Ok(new_parent),
            EmptyPolicy::Keep => write_rewritten_commit(
                repo,
                opts,
                change_ids,
                old,
                old_oid,
                result_tree,
                parent_list,
                author,
                committer,
                message,
            ),
        };
    }

    write_rewritten_commit(
        repo,
        opts,
        change_ids,
        old,
        old_oid,
        result_tree,
        parent_list,
        author,
        committer,
        message,
    )
}

/// Replay with author/message overrides and an explicit parent list (implicit tree).
#[allow(clippy::too_many_arguments)]
fn replay_edited_commit(
    repo: &Repository,
    change_ids: &ChangeIdWriteContext,
    old: &CommitData,
    old_oid: ObjectId,
    parent_list: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
    merge_favor: MergeFavor,
    opts: &RewriteOptions,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    let committer = if opts.preserve_committer {
        old.committer.as_str()
    } else {
        committer
    };
    let empty = empty_tree_oid(repo)?;
    let old_parent = old.parents.first().copied().unwrap_or(empty);
    let new_parent = parent_list.first().copied().unwrap_or(empty);
    let old_parent_tree = if old.parents.is_empty() {
        empty
    } else {
        commit_tree(repo, old_parent)?
    };
    let new_parent_tree = if parent_list.is_empty() {
        empty
    } else {
        commit_tree(repo, new_parent)?
    };

    let result_tree = compute_replayed_tree(
        repo,
        old,
        old_oid,
        old_parent_tree,
        new_parent_tree,
        merge_favor,
        replay_session,
    )?;
    write_replacement_commit(
        repo,
        opts,
        change_ids,
        old,
        old_oid,
        result_tree,
        parent_list,
        author,
        committer,
        message,
        false,
    )
}

fn replay_parent_pair(
    repo: &Repository,
    old: &CommitData,
    mapping: &HashMap<ObjectId, ObjectId>,
    extra_parent: Option<ObjectId>,
) -> Result<(ObjectId, ObjectId), HistoryError> {
    let empty = empty_tree_oid(repo)?;
    if old.parents.is_empty() {
        let new_parent = extra_parent.unwrap_or(empty);
        return Ok((empty, new_parent));
    }
    let old_parent = old.parents[0];
    let new_parent =
        extra_parent.unwrap_or_else(|| mapping.get(&old_parent).copied().unwrap_or(old_parent));
    Ok((old_parent, new_parent))
}

fn empty_tree_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    repo.odb().empty_tree_oid().map_err(library_err)
}

fn compute_replayed_tree(
    repo: &Repository,
    old: &CommitData,
    old_oid: ObjectId,
    old_parent_tree: ObjectId,
    new_parent_tree: ObjectId,
    merge_favor: MergeFavor,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    transplant_tree_across_parent_move(
        repo,
        old_oid,
        &replay_conflict_labels(old_oid, old),
        old.tree,
        old_parent_tree,
        new_parent_tree,
        merge_favor,
        replay_session,
    )
}

/// Three-way merge `subject_tree` from `old_parent_tree` onto `new_parent_tree`, labelling
/// conflict markers with `labels` for the replayed `commit`.
#[allow(clippy::too_many_arguments)]
fn transplant_tree_across_parent_move(
    repo: &Repository,
    commit: ObjectId,
    labels: &ReplayConflictLabels,
    subject_tree: ObjectId,
    old_parent_tree: ObjectId,
    new_parent_tree: ObjectId,
    merge_favor: MergeFavor,
    replay_session: &mut ReplayMergeSession,
) -> Result<ObjectId, HistoryError> {
    replay_session.transplant_tree(
        repo,
        commit,
        labels,
        subject_tree,
        old_parent_tree,
        new_parent_tree,
        merge_favor,
        |commit, merge| {
            let err = conflict_error(repo, commit, &merge);
            if REPLAY_STOP_COMMIT.with(|c| c.get()) == Some(commit) {
                if let Some(replayed_parent) = REPLAY_STOP_PARENT.with(|p| p.get()) {
                    note_resumable_conflict(commit, replayed_parent, merge);
                }
            }
            Err(err)
        },
    )
}

/// [`HistoryError::Conflict`] for `commit` from a conflicted tree merge, with no refs attributed.
pub(crate) fn conflict_error(
    repo: &Repository,
    commit: ObjectId,
    merge: &TreeMergeOutput,
) -> HistoryError {
    // The conflict is the error worth reporting; an unreadable subject only leaves it blank.
    let subject = load_commit(repo, commit)
        .map(|c| commit_message_subject_line(&c.message))
        .unwrap_or_default();
    HistoryError::Conflict {
        commit,
        subject,
        conflicts: crate::operation::conflicts_from_tree_merge(merge),
        refs: Vec::new(),
    }
}

/// Names the scoped branches containing a conflicting commit that is not on `HEAD`'s history,
/// so the failure points at the branch that could not be moved. Other errors pass through.
fn attribute_conflict_to_refs(
    repo: &Repository,
    err: HistoryError,
    scoped_refs: &[(String, ObjectId)],
) -> HistoryError {
    let HistoryError::Conflict {
        commit,
        subject,
        conflicts,
        refs: _,
    } = err
    else {
        return err;
    };
    let on_head = repo
        .head_commit()
        .ok()
        .is_none_or(|head| is_ancestor(repo, commit, head).unwrap_or(true));
    let mut refs: Vec<String> = if on_head {
        Vec::new()
    } else {
        scoped_refs
            .iter()
            .filter(|(_, tip)| is_ancestor(repo, commit, *tip).unwrap_or(false))
            .map(|(name, _)| name.clone())
            .collect()
    };
    refs.sort();
    refs.dedup();
    HistoryError::Conflict {
        commit,
        subject,
        conflicts,
        refs,
    }
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, HistoryError> {
    repo.odb().commit_tree(&commit).map_err(library_err)
}

#[allow(clippy::too_many_arguments)]
fn write_new_commit(
    repo: &Repository,
    opts: &RewriteOptions,
    change_ids: &ChangeIdWriteContext,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
    extra_headers: &[u8],
) -> Result<ObjectId, HistoryError> {
    let mut data = CommitData {
        tree,
        parents: parents.to_vec(),
        author: author.to_owned(),
        committer: committer.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: message.to_owned(),
        raw_message: None,
        extra_headers: extra_headers.to_vec(),
    };
    if let Some(id) = change_ids.policy.id_for_new_commit(
        &data,
        change_ids.hash_algo,
        change_ids.generator.as_ref(),
    )? {
        set_change_id(&mut data, &id);
    }
    write_commit_data(repo, opts, &data)
}

/// Write a replacement commit, preserving encoding and raw author/message bytes from `template`
/// when those fields are unchanged while always applying the new committer.
#[allow(clippy::too_many_arguments)]
fn write_rewritten_commit(
    repo: &Repository,
    opts: &RewriteOptions,
    _change_ids: &ChangeIdWriteContext,
    template: &CommitData,
    template_oid: ObjectId,
    tree: ObjectId,
    parents: &[ObjectId],
    author: &str,
    committer: &str,
    message: &str,
) -> Result<ObjectId, HistoryError> {
    let author_unchanged = author == template.author;
    let committer_unchanged = committer == template.committer;
    let message_unchanged = message == template.message;
    let encoding = if message_unchanged {
        template.encoding.clone()
    } else {
        None
    };
    let raw_message = if message_unchanged {
        template.raw_message.clone()
    } else {
        None
    };
    let generated = identity_raw_for_serialized_commit(&encoding, author, committer);
    let author_raw = if author_unchanged && !template.author_raw.is_empty() {
        template.author_raw.clone()
    } else {
        generated.0
    };
    let committer_raw = if committer_unchanged && !template.committer_raw.is_empty() {
        template.committer_raw.clone()
    } else {
        generated.1
    };
    let extra_headers = extra_headers_without_commit_signatures(&template.extra_headers);
    let mut data = CommitData {
        tree,
        parents: parents.to_vec(),
        author: author.to_owned(),
        committer: committer.to_owned(),
        author_raw,
        committer_raw,
        encoding,
        message: message.to_owned(),
        raw_message,
        extra_headers,
    };
    preserve_logical_change_id(repo, template, template_oid, &mut data)?;
    write_commit_data(repo, opts, &data)
}

fn preserve_logical_change_id(
    repo: &Repository,
    template: &CommitData,
    template_oid: ObjectId,
    data: &mut CommitData,
) -> Result<(), HistoryError> {
    let config = repo.config().map_err(library_err)?;
    if !ChangeIdPolicy::from_config(config.as_ref()).enabled() {
        return Ok(());
    }
    let id = change_id(template).unwrap_or_else(|| ChangeId::derived_from_commit_id(&template_oid));
    set_change_id(data, &id);
    Ok(())
}

fn write_commit_data(
    repo: &Repository,
    opts: &RewriteOptions,
    data: &CommitData,
) -> Result<ObjectId, HistoryError> {
    let bytes = if opts.sign {
        let config = repo.config().map_err(library_err)?;
        prepare_commit_bytes(repo, config.as_ref(), data, None).map_err(signing_err)?
    } else {
        serialize_commit(data)
    };
    repo.odb()
        .write(ObjectKind::Commit, &bytes)
        .map_err(library_err)
}

fn signing_err(err: Error) -> HistoryError {
    HistoryError::Signing {
        message: err.to_string(),
    }
}

fn logical_change_id_for_commit(
    repo: &Repository,
    oid: ObjectId,
) -> Result<Option<ChangeId>, HistoryError> {
    crate::change_id::logical_change_id_for_commit(repo, oid).map_err(library_err)
}

pub(crate) fn plan_ref_updates(
    scoped_refs: &[(String, ObjectId)],
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Vec<PlannedRefUpdate> {
    let mut updates = Vec::new();
    for (name, old_tip) in scoped_refs {
        let new_tip = resolve_mapped_tip(*old_tip, mapping);
        if new_tip != *old_tip {
            updates.push(PlannedRefUpdate {
                name: name.clone(),
                old: *old_tip,
                new: new_tip,
            });
        }
    }
    updates.sort_by(|a, b| a.name.cmp(&b.name));
    updates
}

fn resolve_mapped_tip(tip: ObjectId, mapping: &HashMap<ObjectId, ObjectId>) -> ObjectId {
    mapping.get(&tip).copied().unwrap_or(tip)
}

pub(crate) fn head_effect(
    repo: &Repository,
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Result<HeadEffect, HistoryError> {
    let head = resolve_head_for_repository(repo).map_err(library_err)?;
    let old_oid = match head {
        crate::state::HeadState::Branch { oid, .. } => oid,
        crate::state::HeadState::Detached { oid } => Some(oid),
        crate::state::HeadState::Invalid => None,
    };
    let Some(old_oid) = old_oid else {
        return Ok(HeadEffect::default());
    };
    let new_oid = mapping.get(&old_oid).copied().unwrap_or(old_oid);
    let old_tree = load_commit(repo, old_oid).ok().map(|c| c.tree);
    let new_tree = load_commit(repo, new_oid).ok().map(|c| c.tree);
    let head_ref = match &head {
        crate::state::HeadState::Branch { refname, .. } => Some(refname.clone()),
        crate::state::HeadState::Detached { .. } | crate::state::HeadState::Invalid => None,
    };
    Ok(HeadEffect {
        commit_changes: new_oid != old_oid,
        old_commit: Some(old_oid),
        head_ref,
        old_tree,
        new_tree,
        checkout_onto: None,
    })
}

/// Fast path: fold a contiguous range whose tip is `HEAD` into one commit (no descendant replay).
///
/// Used by [`super::squash::squash`](crate::history::squash::squash) when the range sits at the
/// branch tip and only `HEAD` moves.
pub fn plan_fold_range_at_head(
    repo: &Repository,
    commits: &[ObjectId],
    oldest: ObjectId,
    tip: ObjectId,
    tip_tree: ObjectId,
    message: &str,
    opts: &RewriteOptions,
) -> Result<RewritePlan, HistoryError> {
    if !matches!(opts.scope, RefScope::Head) {
        return Err(HistoryError::Planning {
            message: "head-only fold requires RefScope::Head".into(),
        });
    }
    let change_ids = ChangeIdWriteContext::from_repo(repo)?;
    let head = repo.head_commit()?;
    if tip != head {
        return Err(HistoryError::Planning {
            message: "fold fast path requires the range tip to be HEAD".into(),
        });
    }
    let oldest_commit = load_commit(repo, oldest)?;
    if oldest_commit.parents.is_empty() {
        return Err(HistoryError::RootCommit { commit: oldest });
    }
    let committer_line = opts.committer_line()?;
    let committer = if opts.preserve_committer {
        oldest_commit.committer.as_str()
    } else {
        committer_line.as_str()
    };
    let new_oid = write_replacement_commit(
        repo,
        opts,
        &change_ids,
        &oldest_commit,
        oldest,
        tip_tree,
        &oldest_commit.parents,
        &oldest_commit.author,
        committer,
        message,
        false,
    )?;
    let mut mapping = HashMap::with_capacity(commits.len());
    for &oid in commits {
        mapping.insert(oid, new_oid);
    }
    let squashed_change_id = logical_change_id_for_commit(repo, new_oid)?;
    let squashed_subject = subject_for_rewrite_row(repo, &oldest_commit, Some(new_oid))?;
    let rewritten: Vec<RewrittenCommit> = commits
        .iter()
        .map(|&old| RewrittenCommit {
            old,
            new: Some(new_oid),
            change_id: squashed_change_id.clone(),
            subject: squashed_subject.clone(),
            reused: false,
        })
        .collect();
    let scoped_refs = collect_scoped_refs(repo, opts, oldest)?;
    ensure_branches_not_checked_out_elsewhere(repo, &scoped_refs)?;
    let ref_updates = plan_ref_updates(&scoped_refs, &mapping);
    let head = head_effect(repo, &mapping)?;
    Ok(RewritePlan {
        commits: rewritten,
        ref_updates,
        head,
    })
}

#[cfg(test)]
mod merge_call_tests {
    use super::*;
    use crate::config::environment::RepositoryOptions;
    use crate::index::write_tree::write_tree_from_index;
    use crate::index::{Index, IndexEntry, MODE_REGULAR};
    use crate::refs::write_ref;

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

    fn tree_with_file(
        repo: &Repository,
        parent_tree: ObjectId,
        path: &str,
        bytes: &[u8],
    ) -> crate::error::Result<ObjectId> {
        use crate::porcelain::merge::tree_to_index_entries;
        let blob = repo.odb().write(ObjectKind::Blob, bytes)?;
        let mut index = Index::new();
        index.hash_algo = repo.odb().hash_algo();
        for mut entry in tree_to_index_entries(repo, &parent_tree, "")? {
            entry.base_index_pos = 0;
            index.add_or_replace(entry);
        }
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

    use super::super::replay_merge_session::{
        test_merge_tree_call_count, test_reset_merge_tree_call_count,
        test_reset_upstream_delta_scan_count, test_upstream_delta_scan_count,
    };

    #[test]
    fn reword_parent_only_replay_skips_merge_tree() {
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
        let change_ids = ChangeIdWriteContext::from_repo(&repo).expect("change ids");
        let opts = RewriteOptions {
            scope: RefScope::Branches,
            empty: EmptyPolicy::Drop,
            committer: ident(0).parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        };
        let a = write_new_commit(&repo, &opts, &change_ids, tree_a, &[], &id, &id, "a\n", &[])
            .expect("a");
        let b = write_new_commit(
            &repo,
            &opts,
            &change_ids,
            tree_a,
            &[a],
            &id,
            &id,
            "b\n",
            &[],
        )
        .expect("b");
        write_ref(repo.git_dir(), "refs/heads/main", &b).expect("ref");
        let open =
            Repository::discover(Some(dir.path()), &RepositoryOptions::empty()).expect("open");
        let mut edits = HistoryEdits::new();
        edits.insert(
            a,
            CommitEdit::Rewrite {
                tree: None,
                message: Some("reword\n".into()),
                author: None,
                parents: None,
            },
        );
        test_reset_merge_tree_call_count();
        plan_rewrite(
            &open,
            &edits,
            &RewriteOptions {
                scope: RefScope::Branches,
                empty: EmptyPolicy::Drop,
                committer: ident(2).parse().expect("signature"),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");
        assert_eq!(test_merge_tree_call_count(), 0);
    }

    #[test]
    fn add_only_rebase_stack_skips_three_way_merge() {
        use crate::objects::{serialize_commit, CommitData};
        use crate::revwalk::history::{plan_rebase, RebaseOntoOptions, RebaseRequest};

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let sig = ident(1);
        let fork_tree = blob_tree(&repo, "base.txt", b"base\n").expect("tree");
        let fork_raw = serialize_commit(&CommitData {
            tree: fork_tree,
            parents: vec![],
            author: sig.clone(),
            committer: sig.clone(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "fork\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        let fork = repo
            .odb()
            .write(ObjectKind::Commit, &fork_raw)
            .expect("fork");
        let mut parent = fork;
        let mut onto_tree = fork_tree;
        for i in 0..80 {
            let path = format!("wide/n{i}.txt");
            onto_tree = tree_with_file(&repo, onto_tree, &path, format!("w{i}\n").as_bytes())
                .expect("onto tree");
            let raw = serialize_commit(&CommitData {
                tree: onto_tree,
                parents: vec![parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("wide {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            parent = repo.odb().write(ObjectKind::Commit, &raw).expect("commit");
        }
        let onto = parent;

        let mut stack_parent = fork;
        let mut stack_tree = fork_tree;
        for i in 0..40 {
            let path = format!("team/f{i}.txt");
            stack_tree = tree_with_file(&repo, stack_tree, &path, format!("{i}\n").as_bytes())
                .expect("tree");
            let raw = serialize_commit(&CommitData {
                tree: stack_tree,
                parents: vec![stack_parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("feat {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            stack_parent = repo.odb().write(ObjectKind::Commit, &raw).expect("commit");
        }
        let tip = stack_parent;
        write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: ident(2).parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        let request = RebaseRequest {
            upstream: fork,
            onto: Some(onto),
            branch_tip: tip,
        };
        test_reset_merge_tree_call_count();
        plan_rebase(&repo, &request, &opts).expect("plan");
        assert_eq!(
            test_merge_tree_call_count(),
            0,
            "add-only stack should not call three-way merge per commit"
        );
    }

    #[test]
    fn replay_delete_last_file_in_dir_matches_git_rebase_tree() {
        use crate::objects::parse_commit;
        use crate::revwalk::history::{plan_rebase, RebaseOntoOptions, RebasePlan, RebaseRequest};
        use std::process::Command;

        if Command::new("git").arg("--version").status().is_err() {
            return;
        }

        let root = tempfile::tempdir().expect("tempdir");
        let wt = root.path();
        let git = |args: &[&str]| {
            Command::new("git")
                .current_dir(wt)
                .args(args)
                .env("GIT_AUTHOR_NAME", "T")
                .env("GIT_AUTHOR_EMAIL", "t@grit.test")
                .env("GIT_COMMITTER_NAME", "T")
                .env("GIT_COMMITTER_EMAIL", "t@grit.test")
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .status()
                .expect("git");
        };
        git(&["init", "-b", "main"]);
        std::fs::write(wt.join("upstream.txt"), "v1\n").expect("write");
        git(&["add", "upstream.txt"]);
        git(&["commit", "-m", "root"]);
        let fork = Command::new("git")
            .current_dir(wt)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let fork_hex = String::from_utf8(fork.stdout)
            .expect("utf8")
            .trim()
            .to_owned();
        git(&["branch", "feat"]);
        git(&["switch", "main"]);
        std::fs::write(wt.join("upstream.txt"), "v2\n").expect("write");
        git(&["add", "upstream.txt"]);
        git(&["commit", "-m", "main bump"]);
        let onto_hex = Command::new("git")
            .current_dir(wt)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let onto_hex = String::from_utf8(onto_hex.stdout)
            .expect("utf8")
            .trim()
            .to_owned();
        git(&["switch", "feat"]);
        std::fs::create_dir_all(wt.join("dir")).expect("dir");
        std::fs::write(wt.join("dir/only.txt"), "only\n").expect("write");
        git(&["add", "dir/only.txt"]);
        git(&["commit", "-m", "add dir"]);
        std::fs::remove_file(wt.join("dir/only.txt")).expect("rm");
        git(&["add", "-u"]);
        git(&["commit", "-m", "drop dir"]);
        let tip_hex = Command::new("git")
            .current_dir(wt)
            .args(["rev-parse", "HEAD"])
            .output()
            .expect("rev-parse");
        let tip_hex = String::from_utf8(tip_hex.stdout)
            .expect("utf8")
            .trim()
            .to_owned();

        let fork = ObjectId::from_hex(&fork_hex).expect("fork");
        let onto = ObjectId::from_hex(&onto_hex).expect("onto");
        let tip = ObjectId::from_hex(&tip_hex).expect("tip");
        let repo = Repository::discover(Some(wt), &RepositoryOptions::empty()).expect("open");
        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: ident(2).parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        let request = RebaseRequest {
            upstream: fork,
            onto: Some(onto),
            branch_tip: tip,
        };
        let plan = match plan_rebase(&repo, &request, &opts).expect("plan") {
            RebasePlan::Plan(plan) => plan,
            other => panic!("expected plan, got {other:?}"),
        };
        let new_tip = plan
            .commits
            .last()
            .and_then(|row| row.new)
            .expect("rewritten tip");
        let grit_tree = parse_commit(&repo.odb().read(&new_tip).expect("read").data)
            .expect("parse")
            .tree;

        let git_wt = tempfile::tempdir().expect("git wt");
        Command::new("git")
            .current_dir(git_wt.path())
            .args(["clone", wt.to_str().expect("utf8"), "r"])
            .status()
            .expect("clone");
        let clone = git_wt.path().join("r");
        Command::new("git")
            .current_dir(&clone)
            .args(["rebase", "--onto", &onto_hex, &fork_hex, "feat"])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git rebase");
        let git_tree_hex = Command::new("git")
            .current_dir(&clone)
            .args(["rev-parse", "feat^{tree}"])
            .output()
            .expect("tree");
        let git_tree =
            ObjectId::from_hex(String::from_utf8(git_tree_hex.stdout).expect("utf8").trim())
                .expect("parse tree");

        assert_eq!(
            grit_tree, git_tree,
            "delete replay tree must match git (no empty dir tree)"
        );
    }

    fn rename_friendly_blob(i: usize, tag: &str) -> Vec<u8> {
        format!(
            "rename-friendly file {i}\nshared boilerplate alpha beta gamma delta epsilon\n{tag}\n"
        )
        .into_bytes()
    }

    fn wide_rename_fork_onto(
        repo: &Repository,
        sig: &str,
        file_count: usize,
    ) -> (ObjectId, ObjectId, ObjectId) {
        wide_rename_fork_onto_inner(repo, sig, file_count, false)
    }

    fn wide_rename_fork_onto_pure_renames(
        repo: &Repository,
        sig: &str,
        file_count: usize,
    ) -> (ObjectId, ObjectId, ObjectId) {
        wide_rename_fork_onto_inner(repo, sig, file_count, true)
    }

    fn wide_rename_fork_onto_inner(
        repo: &Repository,
        sig: &str,
        file_count: usize,
        pure_renames: bool,
    ) -> (ObjectId, ObjectId, ObjectId) {
        use crate::objects::{serialize_commit, CommitData};

        let mut fork_tree =
            blob_tree(repo, "shared.toml", b"[package]\nname = \"root\"\n").expect("tree");
        for i in 0..file_count {
            let path = format!("dir/f{i}.txt");
            fork_tree = tree_with_file(&repo, fork_tree, &path, &rename_friendly_blob(i, "fork"))
                .expect("fork wide");
        }
        let fork_raw = serialize_commit(&CommitData {
            tree: fork_tree,
            parents: vec![],
            author: sig.to_owned(),
            committer: sig.to_owned(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "fork\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        let fork = repo
            .odb()
            .write(ObjectKind::Commit, &fork_raw)
            .expect("fork");

        let mut onto_tree = tree_with_file(
            repo,
            fork_tree,
            "shared.toml",
            b"[package]\nname = \"root\"\nversion = \"1\"\n",
        )
        .expect("onto shared edit");
        for i in 0..file_count {
            let old = format!("dir/f{i}.txt");
            let new = format!("moved/f{i}.txt");
            let moved_bytes = if pure_renames {
                rename_friendly_blob(i, "fork")
            } else {
                rename_friendly_blob(i, "onto")
            };
            onto_tree = tree_with_file(&repo, onto_tree, &new, &moved_bytes).expect("moved add");
            onto_tree = tree_without_path(&repo, onto_tree, &old).expect("moved rm");
        }
        let onto_raw = serialize_commit(&CommitData {
            tree: onto_tree,
            parents: vec![fork],
            author: sig.to_owned(),
            committer: sig.to_owned(),
            author_raw: Vec::new(),
            committer_raw: Vec::new(),
            encoding: None,
            message: "onto\n".into(),
            raw_message: None,
            extra_headers: Vec::new(),
        });
        let onto = repo
            .odb()
            .write(ObjectKind::Commit, &onto_raw)
            .expect("onto");
        (fork, onto, fork_tree)
    }

    fn plan_overlap_rebase(
        repo: &Repository,
        fork: ObjectId,
        onto: ObjectId,
        tip: ObjectId,
    ) -> ObjectId {
        use crate::objects::parse_commit;
        use crate::revwalk::history::{plan_rebase, RebaseOntoOptions, RebasePlan, RebaseRequest};

        let opts = RebaseOntoOptions::new(RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: ident(2).parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        });
        let request = RebaseRequest {
            upstream: fork,
            onto: Some(onto),
            branch_tip: tip,
        };
        test_reset_merge_tree_call_count();
        test_reset_upstream_delta_scan_count();
        let plan = match plan_rebase(repo, &request, &opts).expect("plan") {
            RebasePlan::Plan(plan) => plan,
            other => panic!("expected plan, got {other:?}"),
        };
        assert_eq!(
            test_upstream_delta_scan_count(),
            1,
            "fork→onto rename map should be built once per replay session"
        );
        assert_eq!(
            test_merge_tree_call_count(),
            0,
            "overlap replay should use cached upstream renames, not full tree merge per commit"
        );
        let new_tip = plan
            .commits
            .last()
            .and_then(|row| row.new)
            .expect("rewritten tip");
        parse_commit(&repo.odb().read(&new_tip).expect("read").data)
            .expect("parse")
            .tree
    }

    #[test]
    fn rebase_overlap_upstream_modified_shared_and_add_under_moved() {
        use crate::objects::{serialize_commit, CommitData};

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let sig = ident(1);
        let (fork, onto, fork_tree) = wide_rename_fork_onto(&repo, &sig, 80);

        let mut stack_parent = fork;
        let mut stack_tree = fork_tree;
        for i in 0..12 {
            let path = format!("moved/extra-shared-{i}.txt");
            stack_tree =
                tree_with_file(&repo, stack_tree, &path, format!("layer {i}\n").as_bytes())
                    .expect("stack add under moved tree");
            let raw = serialize_commit(&CommitData {
                tree: stack_tree,
                parents: vec![stack_parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("moved extra {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            stack_parent = repo
                .odb()
                .write(ObjectKind::Commit, &raw)
                .expect("stack commit");
        }
        let tip = stack_parent;
        write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        plan_overlap_rebase(&repo, fork, onto, tip);
    }

    #[test]
    fn rebase_overlap_add_under_moved_dir_uses_cached_renames() {
        use crate::objects::{serialize_commit, CommitData};

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let sig = ident(1);
        let (fork, onto, fork_tree) = wide_rename_fork_onto(&repo, &sig, 80);

        let mut stack_parent = fork;
        let mut stack_tree = fork_tree;
        for i in 0..12 {
            let path = format!("moved/stack-{i}.txt");
            stack_tree = tree_with_file(&repo, stack_tree, &path, format!("{i}\n").as_bytes())
                .expect("stack add under moved");
            let raw = serialize_commit(&CommitData {
                tree: stack_tree,
                parents: vec![stack_parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("moved add {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            stack_parent = repo
                .odb()
                .write(ObjectKind::Commit, &raw)
                .expect("stack commit");
        }
        let tip = stack_parent;
        write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        plan_overlap_rebase(&repo, fork, onto, tip);
    }

    #[test]
    fn rebase_overlap_add_under_renamed_dir_uses_cached_renames() {
        use crate::objects::{serialize_commit, CommitData};

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let sig = ident(1);
        let (fork, onto, fork_tree) = wide_rename_fork_onto(&repo, &sig, 80);

        let mut stack_parent = fork;
        let mut stack_tree = fork_tree;
        for i in 0..12 {
            let path = format!("dir/feat-{i}.txt");
            stack_tree = tree_with_file(&repo, stack_tree, &path, format!("new {i}\n").as_bytes())
                .expect("stack add under dir");
            let raw = serialize_commit(&CommitData {
                tree: stack_tree,
                parents: vec![stack_parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("under dir {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            stack_parent = repo
                .odb()
                .write(ObjectKind::Commit, &raw)
                .expect("stack commit");
        }
        let tip = stack_parent;
        write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        plan_overlap_rebase(&repo, fork, onto, tip);
    }

    #[test]
    fn rebase_overlap_edit_pre_rename_path_uses_cached_renames() {
        use crate::objects::{serialize_commit, CommitData};

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let sig = ident(1);
        let (fork, onto, fork_tree) = wide_rename_fork_onto_pure_renames(&repo, &sig, 80);

        let mut stack_parent = fork;
        let mut stack_tree = fork_tree;
        for i in 0..12 {
            let file_idx = i % 80;
            let path = format!("dir/f{file_idx}.txt");
            let mut bytes = rename_friendly_blob(file_idx, "fork");
            bytes.extend_from_slice(format!("# stack note {i}\n").as_bytes());
            stack_tree = tree_with_file(&repo, stack_tree, &path, &bytes)
                .expect("stack edit pre-rename path");
            let raw = serialize_commit(&CommitData {
                tree: stack_tree,
                parents: vec![stack_parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("edit pre-move {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            stack_parent = repo
                .odb()
                .write(ObjectKind::Commit, &raw)
                .expect("stack commit");
        }
        let tip = stack_parent;
        write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        plan_overlap_rebase(&repo, fork, onto, tip);
    }

    #[test]
    #[ignore = "release-profile timing for reviewer fixture (#2271)"]
    fn rebase_overlap_reviewer_fixture_timing() {
        use crate::objects::{serialize_commit, CommitData};
        use std::time::Instant;

        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(
            dir.path(),
            &crate::repo::InitOptions::new()
                .with_initial_branch("main".to_owned())
                .with_ref_storage(crate::RefStorageFormat::Files),
        )
        .expect("init");
        let sig = ident(1);
        let (fork, onto, fork_tree) = wide_rename_fork_onto(&repo, &sig, 500);
        let mut stack_parent = fork;
        let mut stack_tree = fork_tree;
        for i in 0..20 {
            let path = format!("moved/stack-{i}.txt");
            stack_tree = tree_with_file(&repo, stack_tree, &path, format!("edit {i}\n").as_bytes())
                .expect("stack");
            let raw = serialize_commit(&CommitData {
                tree: stack_tree,
                parents: vec![stack_parent],
                author: sig.clone(),
                committer: sig.clone(),
                author_raw: Vec::new(),
                committer_raw: Vec::new(),
                encoding: None,
                message: format!("feat {i}\n"),
                raw_message: None,
                extra_headers: Vec::new(),
            });
            stack_parent = repo
                .odb()
                .write(ObjectKind::Commit, &raw)
                .expect("stack commit");
        }
        let tip = stack_parent;
        write_ref(repo.git_dir(), "refs/heads/main", &tip).expect("ref");
        test_reset_merge_tree_call_count();
        test_reset_upstream_delta_scan_count();
        let start = Instant::now();
        plan_overlap_rebase(&repo, fork, onto, tip);
        eprintln!(
            "reviewer fixture plan: {:.3}s",
            start.elapsed().as_secs_f64()
        );
        eprintln!(
            "merge calls {} upstream scans {}",
            test_merge_tree_call_count(),
            test_upstream_delta_scan_count()
        );
    }

    fn tree_without_path(
        repo: &Repository,
        parent_tree: ObjectId,
        path: &str,
    ) -> crate::error::Result<ObjectId> {
        use crate::porcelain::merge::tree_to_index_entries;
        let mut index = Index::new();
        index.hash_algo = repo.odb().hash_algo();
        let path_bytes = path.as_bytes();
        for mut entry in tree_to_index_entries(repo, &parent_tree, "")? {
            if entry.path == path_bytes {
                continue;
            }
            entry.base_index_pos = 0;
            index.add_or_replace(entry);
        }
        write_tree_from_index(repo.odb(), &index, "")
    }
}
