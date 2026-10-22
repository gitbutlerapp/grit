//! In-memory history rewrite planning.

use std::collections::{HashMap, HashSet};
#[cfg(any(test, feature = "unstable-bench-helpers"))]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use crate::change_id::{
    change_id, set_change_id, ChangeId, ChangeIdError, ChangeIdGenerator, ChangeIdPolicy,
};
use crate::commit::signing::extra_headers_without_commit_signatures;
use crate::commit_encoding::identity_raw_for_serialized_commit;
use crate::error::Error;
use crate::merge_base::is_ancestor;
use crate::merge_file::MergeFavor;
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, serialize_commit, CommitData, HashAlgo, ObjectId, ObjectKind};
use crate::porcelain::commit::prepare_commit_bytes;
use crate::repo::Repository;
use crate::revwalk::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::state::resolve_head;
use crate::worktree::{find_worktree_with_branch_checked_out, worktree_admin_dir};

use super::error::HistoryError;
use super::types::{
    CommitEdit, EmptyPolicy, HeadEffect, HistoryEdits, PlannedRefUpdate, RefScope, RewriteOptions,
    RewritePlan, RewrittenCommit,
};

#[cfg(any(test, feature = "unstable-bench-helpers"))]
static MERGE_TREE_CALLS: AtomicUsize = AtomicUsize::new(0); // hygiene: merge-call counter for tests and unstable-bench-helpers

/// Test hook: number of three-way tree merges performed by planning.
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[doc(hidden)]
#[must_use]
pub fn test_merge_tree_call_count() -> usize {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    {
        MERGE_TREE_CALLS.load(Ordering::Relaxed)
    }
    #[cfg(not(any(test, feature = "unstable-bench-helpers")))]
    {
        0
    }
}

/// Reset [`test_merge_tree_call_count`].
#[cfg(any(test, feature = "unstable-bench-helpers"))]
#[doc(hidden)]
pub fn test_reset_merge_tree_call_count() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    MERGE_TREE_CALLS.store(0, Ordering::Relaxed);
}

fn record_merge_call() {
    #[cfg(any(test, feature = "unstable-bench-helpers"))]
    MERGE_TREE_CALLS.fetch_add(1, Ordering::Relaxed);
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

fn change_id_err(err: ChangeIdError) -> HistoryError {
    HistoryError::Planning {
        message: err.to_string(),
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
    plan_rewrite_with_order(repo, edits, opts, None, false)
}

/// Like [`plan_rewrite`], but when `replay_order` is set, replays commits in that exact
/// topological order (used by [`super::rebase::rebase_onto`](crate::history::rebase::rebase_onto)
/// so merge side branches are included).
pub(crate) fn plan_rewrite_with_order(
    repo: &Repository,
    edits: &HistoryEdits,
    opts: &RewriteOptions,
    replay_order: Option<&[ObjectId]>,
    flatten_leaf_merges: bool,
) -> Result<RewritePlan, HistoryError> {
    if edits.is_empty() {
        return Ok(RewritePlan {
            commits: Vec::new(),
            ref_updates: Vec::new(),
            head: HeadEffect::default(),
        });
    }

    let change_ids = ChangeIdWriteContext::from_repo(repo)?;

    let oldest = oldest_edited_commit(repo, edits)?;
    let scoped_refs = collect_scoped_refs(repo, opts, oldest)?;
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

    if matches!(opts.scope, RefScope::Head) {
        let head_oid = head_oid(repo)?;
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

    let mut mapping: HashMap<ObjectId, ObjectId> = HashMap::new();
    let mut rewritten: Vec<RewrittenCommit> = Vec::with_capacity(commits_to_replay.len());

    for old_oid in commits_to_replay {
        let old = load_commit(repo, old_oid)?;
        let edit = edits.get(&old_oid);

        if matches!(
            edit,
            Some(CommitEdit::InsertBefore(_)) | Some(CommitEdit::SplitBefore { .. })
        ) && old.parents.len() > 1
        {
            return Err(HistoryError::InsertBeforeOnMerge { commit: old_oid });
        }

        if let Some(CommitEdit::Reuse(existing)) = edit {
            mapping.insert(old_oid, *existing);
            rewritten.push(RewrittenCommit {
                old: old_oid,
                new: Some(*existing),
                change_id: logical_change_id_for_commit(repo, *existing)?,
            });
            continue;
        }

        if matches!(edit, Some(CommitEdit::Drop)) {
            if old.parents.is_empty() {
                return Err(HistoryError::RootCommit { commit: old_oid });
            }
            let parent = if flatten_leaf_merges && old.parents.len() > 1 {
                mapping
                    .get(&old.parents[1])
                    .copied()
                    .unwrap_or(old.parents[1])
            } else {
                mapped_first_parent(&mapping, &old, old_oid)?
            };
            mapping.insert(old_oid, parent);
            rewritten.push(RewrittenCommit {
                old: old_oid,
                new: None,
                change_id: None,
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
                    &opts.committer,
                    &insert.message,
                    &[],
                )?)
            }
            _ => None,
        };

        let (new_oid, new_field) = match edit {
            None => {
                let oid = replay_commit_descendant(
                    repo,
                    &change_ids,
                    &mapping,
                    &old,
                    old_oid,
                    insert_parent,
                    opts,
                )?;
                (oid, Some(oid))
            }
            Some(CommitEdit::InsertBefore(_)) => {
                let oid = replay_commit_descendant(
                    repo,
                    &change_ids,
                    &mapping,
                    &old,
                    old_oid,
                    insert_parent,
                    opts,
                )?;
                (oid, Some(oid))
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
                let oid = write_rewritten_commit(
                    repo,
                    opts,
                    &change_ids,
                    &old,
                    old_oid,
                    old.tree,
                    &parent_list,
                    &old.author,
                    &opts.committer,
                    message,
                )?;
                (oid, Some(oid))
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
                        opts,
                        Some(&parent_list),
                    )?
                } else {
                    replay_edited_commit(
                        repo,
                        &change_ids,
                        &old,
                        old_oid,
                        &parent_list,
                        &old.author,
                        &opts.committer,
                        &old.message,
                        opts,
                    )?
                };
                (oid, Some(oid))
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
                let oid = if let Some(explicit_tree) = *tree {
                    write_rewritten_commit(
                        repo,
                        opts,
                        &change_ids,
                        &old,
                        old_oid,
                        explicit_tree,
                        &parent_list,
                        author,
                        &opts.committer,
                        message,
                    )?
                } else {
                    replay_edited_commit(
                        repo,
                        &change_ids,
                        &old,
                        old_oid,
                        &parent_list,
                        author,
                        &opts.committer,
                        message,
                        opts,
                    )?
                };
                (oid, Some(oid))
            }
            Some(CommitEdit::Drop) | Some(CommitEdit::Reuse(_)) => unreachable!("handled above"),
        };

        mapping.insert(old_oid, new_oid);
        rewritten.push(RewrittenCommit {
            old: old_oid,
            new: new_field,
            change_id: match new_field {
                Some(oid) => logical_change_id_for_commit(repo, oid)?,
                None => None,
            },
        });
    }

    let ref_updates = plan_ref_updates(&scoped_refs, &mapping);
    let head = head_effect(repo, &mapping)?;

    Ok(RewritePlan {
        commits: rewritten,
        ref_updates,
        head,
    })
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
        let head_oid = head_oid(repo)?;
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
            &opts.committer,
            &old.message,
            opts,
        )?;
        mapping.insert(old_oid, new_oid);
        parent = Some(new_oid);
    }

    let rewritten = commits_to_replay
        .iter()
        .map(|&old| {
            let new = mapping.get(&old).copied().unwrap_or(old);
            Ok(RewrittenCommit {
                old,
                new: Some(new),
                change_id: logical_change_id_for_commit(repo, new)?,
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
    if let Ok(head_oid) = head_oid(repo) {
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

fn head_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
    head.oid().copied().ok_or(HistoryError::NotInHistory {
        commit: ObjectId::null(repo.odb().hash_algo()),
    })
}

pub(crate) fn collect_scoped_refs(
    repo: &Repository,
    opts: &RewriteOptions,
    oldest: ObjectId,
) -> Result<Vec<(String, ObjectId)>, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
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
            let branches = repo.references().list("refs/heads/").map_err(library_err)?;
            for (name, oid) in branches {
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
                worktree: found.worktree_path.display().to_string(),
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
    let raw = repo.odb().read(&oid).map_err(library_err)?;
    if raw.kind != ObjectKind::Commit {
        return Err(library_err(Error::CorruptObject(format!(
            "expected commit, got {}",
            raw.kind.as_str()
        ))));
    }
    parse_commit(&raw.data).map_err(library_err)
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

fn replay_commit_descendant(
    repo: &Repository,
    change_ids: &ChangeIdWriteContext,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    old_oid: ObjectId,
    extra_parent: Option<ObjectId>,
    opts: &RewriteOptions,
) -> Result<ObjectId, HistoryError> {
    if old.parents.len() > 1 {
        return replay_merge_descendant(
            repo,
            change_ids,
            mapping,
            old,
            old_oid,
            extra_parent,
            opts,
            None,
        );
    }
    replay_descendant(repo, change_ids, mapping, old, old_oid, extra_parent, opts)
}

fn replay_descendant(
    repo: &Repository,
    change_ids: &ChangeIdWriteContext,
    mapping: &HashMap<ObjectId, ObjectId>,
    old: &CommitData,
    old_oid: ObjectId,
    extra_parent: Option<ObjectId>,
    opts: &RewriteOptions,
) -> Result<ObjectId, HistoryError> {
    let (_, new_parent) = replay_parent_pair(repo, old, mapping, extra_parent)?;
    replay_edited_commit(
        repo,
        change_ids,
        old,
        old_oid,
        &[new_parent],
        &old.author,
        &opts.committer,
        &old.message,
        opts,
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
    opts: &RewriteOptions,
    parent_list_override: Option<&[ObjectId]>,
) -> Result<ObjectId, HistoryError> {
    let parent_list = parent_list_override
        .map(<[_]>::to_vec)
        .unwrap_or_else(|| mapped_merge_parents(old, mapping, extra_parent));
    let result_tree = replay_merge_tree(repo, old, old_oid, &parent_list)?;

    let committer = if opts.preserve_committer {
        old.committer.as_str()
    } else {
        opts.committer.as_str()
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
) -> Result<ObjectId, HistoryError> {
    if old.parents.len() != new_parents.len() {
        return Err(HistoryError::Planning {
            message: "merge replay parent count mismatch".into(),
        });
    }

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
            result_tree,
            old_parent_tree,
            new_parent_tree,
        )?;
    }
    Ok(result_tree)
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
    opts: &RewriteOptions,
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

    let result_tree = compute_replayed_tree(repo, old, old_oid, old_parent_tree, new_parent_tree)?;

    if old.tree != old_parent_tree && result_tree == new_parent_tree {
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
    repo.odb().write(ObjectKind::Tree, &[]).map_err(library_err)
}

fn compute_replayed_tree(
    repo: &Repository,
    old: &CommitData,
    old_oid: ObjectId,
    old_parent_tree: ObjectId,
    new_parent_tree: ObjectId,
) -> Result<ObjectId, HistoryError> {
    transplant_tree_across_parent_move(repo, old_oid, old.tree, old_parent_tree, new_parent_tree)
}

fn transplant_tree_across_parent_move(
    repo: &Repository,
    commit: ObjectId,
    subject_tree: ObjectId,
    old_parent_tree: ObjectId,
    new_parent_tree: ObjectId,
) -> Result<ObjectId, HistoryError> {
    if old_parent_tree == new_parent_tree {
        return Ok(subject_tree);
    }
    record_merge_call();
    let merge = merge_trees_three_way(
        repo,
        old_parent_tree,
        new_parent_tree,
        subject_tree,
        MergeFavor::default(),
        WhitespaceMergeOptions::default(),
        None,
        TreeMergeConflictPresentation::default(),
    )
    .map_err(library_err)?;

    if merge.index.has_unmerged_entries() || !merge.conflict_content.is_empty() {
        return Err(HistoryError::Conflict {
            commit,
            paths: conflict_paths(&merge),
        });
    }
    Ok(merge.result_tree)
}

fn conflict_paths(merge: &crate::merge_trees::TreeMergeOutput) -> Vec<String> {
    let mut paths = HashSet::new();
    for key in merge.conflict_content.keys() {
        paths.insert(String::from_utf8_lossy(key).into_owned());
    }
    for entry in &merge.index.entries {
        if entry.stage() != 0 {
            paths.insert(String::from_utf8_lossy(&entry.path).into_owned());
        }
    }
    paths.into_iter().collect()
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, HistoryError> {
    Ok(load_commit(repo, commit)?.tree)
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
    if let Some(id) = change_ids
        .policy
        .id_for_new_commit(&data, change_ids.hash_algo, change_ids.generator.as_ref())
        .map_err(change_id_err)?
    {
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
    let (author_raw, committer_raw) = if author_unchanged && !template.author_raw.is_empty() {
        let (_, committer_raw) = identity_raw_for_serialized_commit(&encoding, author, committer);
        (template.author_raw.clone(), committer_raw)
    } else {
        identity_raw_for_serialized_commit(&encoding, author, committer)
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

fn plan_ref_updates(
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

fn head_effect(
    repo: &Repository,
    mapping: &HashMap<ObjectId, ObjectId>,
) -> Result<HeadEffect, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(library_err)?;
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
    let head = head_oid(repo)?;
    if tip != head {
        return Err(HistoryError::Planning {
            message: "fold fast path requires the range tip to be HEAD".into(),
        });
    }
    let oldest_commit = load_commit(repo, oldest)?;
    if oldest_commit.parents.is_empty() {
        return Err(HistoryError::RootCommit { commit: oldest });
    }
    let new_oid = write_rewritten_commit(
        repo,
        opts,
        &change_ids,
        &oldest_commit,
        oldest,
        tip_tree,
        &oldest_commit.parents,
        &oldest_commit.author,
        &opts.committer,
        message,
    )?;
    let mut mapping = HashMap::with_capacity(commits.len());
    for &oid in commits {
        mapping.insert(oid, new_oid);
    }
    let squashed_change_id = logical_change_id_for_commit(repo, new_oid)?;
    let rewritten: Vec<RewrittenCommit> = commits
        .iter()
        .map(|&old| RewrittenCommit {
            old,
            new: Some(new_oid),
            change_id: squashed_change_id.clone(),
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
            committer: ident(0),
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
                committer: ident(2),
                preserve_committer: false,
                sign: true,
            },
        )
        .expect("plan");
        assert_eq!(test_merge_tree_call_count(), 0);
    }
}
