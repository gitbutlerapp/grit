//! Stop/resume rebase sessions with Git-compatible `.git/rebase-merge/` state.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::lockfile::{LockFile, LockWait};
use crate::objects::{parse_commit, ObjectId};
use crate::operation::conflicts_from_tree_merge;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::index_from_tree::index_from_tree_with_worktree_stats;
use crate::porcelain::merge::apply_conflict_merge_worktree;
use crate::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use crate::refs;
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};
use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

use super::apply::apply_rewrite;
use super::error::HistoryError;
use super::plan::{plan_rewrite_with_order_resumable_seeded, ResumablePlanOutcome};
use super::rebase::{plan_rebase_resumable, RebaseOntoOptions, RebasePlan, RebaseRequest};
use super::types::{
    AppliedRewrite, ApplyOptions, CommitEdit, HistoryEdits, PlannedRefUpdate, RewriteOptions,
    RewritePlan, RewrittenCommit,
};
use crate::operation::Conflict;

/// Options for [`start_rebase`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRebaseOptions {
    /// Upstream, optional onto, and branch tip to rebase.
    pub request: RebaseRequest,
    /// Replay and cherry-detection options.
    pub rebase: RebaseOntoOptions,
    /// Reflog identity and message used when the rebase finishes with [`apply_rewrite`].
    pub apply: ApplyOptions,
}

/// Snapshot when a resumable rebase stops on conflicts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebaseStop {
    /// Original commit that could not be replayed cleanly.
    pub commit: ObjectId,
    /// Conflicted paths and kinds for the current step.
    pub conflicts: Vec<Conflict>,
    /// Source commits fully replayed before the stop (oldest first).
    pub done: Vec<ObjectId>,
    /// Source commits not yet replayed (including `commit`).
    pub remaining: Vec<ObjectId>,
}

/// Result of starting or advancing a rebase session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RebaseOutcome {
    /// Every commit was replayed and ref updates were applied.
    Done {
        /// Ref movements performed on completion.
        applied: AppliedRewrite,
        /// Final rewrite plan (commits and ref updates).
        plan: RewritePlan,
    },
    /// Replay stopped on conflicts; `.git/rebase-merge/` and grit state were written.
    Stopped(RebaseStop),
}

/// Failures while driving a rebase session.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum RebaseSessionError {
    /// No rebase session is in progress.
    #[error("no rebase in progress")]
    NoRebaseInProgress,

    /// A rebase is already in progress.
    #[error("rebase is already in progress")]
    RebaseAlreadyInProgress,

    /// The index still has unmerged entries.
    #[error("cannot continue: unmerged paths remain in the index")]
    UnmergedPaths,

    /// Nothing to commit after resolving conflicts.
    #[error("nothing to commit after conflict resolution")]
    NothingToCommit,

    /// Branch rebase requires a symbolic `HEAD`.
    #[error("rebase session requires HEAD on a branch")]
    NotOnBranch,

    /// Underlying history rewrite failure.
    #[error(transparent)]
    History(#[from] HistoryError),

    /// Repository I/O or ref failure.
    #[error(transparent)]
    Library(#[from] crate::error::Error),
}

/// Start a resumable rebase of `opts.request.branch_tip` onto the effective onto commit.
///
/// On a clean replay, applies the plan atomically and returns [`RebaseOutcome::Done`].
/// On conflict, writes Git-compatible rebase state and returns [`RebaseOutcome::Stopped`]
/// without moving the branch ref away from its pre-rebase tip.
///
/// # Errors
///
/// Returns [`RebaseSessionError`] when another rebase is active, `HEAD` is not on a branch,
/// or planning/apply fails.
pub fn start_rebase(
    repo: &Repository,
    opts: &StartRebaseOptions,
) -> Result<RebaseOutcome, RebaseSessionError> {
    if rebase_session_active(repo.git_dir())? {
        return Err(RebaseSessionError::RebaseAlreadyInProgress);
    }
    let (head_ref, orig_head) = branch_head(repo)?;
    match plan_rebase_resumable(repo, &opts.request, &opts.rebase)? {
        Err(RebasePlan::UpToDate) => Ok(RebaseOutcome::Done {
            applied: AppliedRewrite {
                ref_updates: Vec::new(),
                head: orig_head,
            },
            plan: RewritePlan {
                commits: Vec::new(),
                ref_updates: Vec::new(),
                head: super::types::HeadEffect::default(),
            },
        }),
        Err(RebasePlan::Plan(_) | RebasePlan::Conflict { .. }) => Err(HistoryError::Planning {
            message: "unexpected non-resumable rebase plan".into(),
        }
        .into()),
        Ok(ResumablePlanOutcome::Complete(plan)) => {
            let applied = apply_rewrite(repo, &plan, &opts.apply)?;
            Ok(RebaseOutcome::Done { applied, plan })
        }
        Ok(ResumablePlanOutcome::Partial(partial)) => {
            let stop = install_stopped_rebase(
                repo,
                &head_ref,
                orig_head,
                &opts.request,
                &opts.rebase,
                &opts.apply,
                *partial,
            )?;
            Ok(RebaseOutcome::Stopped(stop))
        }
    }
}

/// Finish the current conflicted commit and replay remaining commits.
///
/// # Errors
///
/// Returns [`RebaseSessionError::UnmergedPaths`] when the index still records conflicts.
pub fn continue_rebase(
    repo: &Repository,
    opts: &StartRebaseOptions,
) -> Result<RebaseOutcome, RebaseSessionError> {
    let state = load_persisted_state(repo.git_dir())?;
    if repo.load_index()?.has_unmerged_entries() {
        return Err(RebaseSessionError::UnmergedPaths);
    }
    let current = state
        .remaining
        .first()
        .copied()
        .ok_or(RebaseSessionError::NoRebaseInProgress)?;
    let new_tree = {
        let mut index = repo.load_index()?;
        write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
            .map_err(map_lib)?
    };
    let head_oid = resolve_head_oid(repo)?;
    let head_tree = commit_tree(repo, head_oid)?;
    if new_tree == head_tree {
        return Err(RebaseSessionError::NothingToCommit);
    }

    let mut edits = state.edits.clone();
    edits.insert(
        current,
        CommitEdit::Rewrite {
            tree: Some(new_tree),
            message: None,
            author: None,
            parents: Some(vec![head_oid]),
        },
    );

    let seed = (state.mapping.clone(), state.rewritten.clone());
    let rewrite = replan_rewrite_options(&state);
    let outcome = plan_rewrite_with_order_resumable_seeded(
        repo,
        &edits,
        &rewrite,
        Some(&state.replay_order),
        state.flatten_leaf_merges,
        Some(seed),
        Some(state.scoped_refs.clone()),
    )?;
    advance_after_plan(repo, &state, &opts.apply, outcome, edits)
}

/// Skip the current todo commit and replay the rest.
pub fn skip_rebase(
    repo: &Repository,
    opts: &StartRebaseOptions,
) -> Result<RebaseOutcome, RebaseSessionError> {
    let state = load_persisted_state(repo.git_dir())?;
    let current = state
        .remaining
        .first()
        .copied()
        .ok_or(RebaseSessionError::NoRebaseInProgress)?;
    let head_oid = resolve_head_oid(repo)?;
    reset_index_worktree_to_head(repo, head_oid)?;

    let replayed_parent = head_oid;
    let mut mapping = state.mapping.clone();
    mapping.insert(current, replayed_parent);
    let mut rewritten = state.rewritten.clone();
    rewritten.push(RewrittenCommit {
        old: current,
        new: None,
        change_id: None,
        subject: None,
    });

    let rewrite = replan_rewrite_options(&state);
    let outcome = plan_rewrite_with_order_resumable_seeded(
        repo,
        &state.edits,
        &rewrite,
        Some(&state.replay_order),
        state.flatten_leaf_merges,
        Some((mapping, rewritten)),
        Some(state.scoped_refs.clone()),
    )?;
    advance_after_plan(repo, &state, &opts.apply, outcome, state.edits.clone())
}

/// Cancel the in-progress rebase and restore the branch to `orig-head`.
pub fn abort_rebase(repo: &Repository, identity: &str) -> Result<(), RebaseSessionError> {
    let state = load_persisted_state(repo.git_dir())?;
    let orig_head = state.orig_head;
    hard_reset_branch(repo, &state.head_ref, orig_head, identity)?;
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", &state.head_ref).map_err(map_lib)?;
    clear_rebase_state(repo.git_dir())?;
    Ok(())
}

// --- persisted session ---

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRebaseState {
    mapping: HashMap<String, String>,
    rewritten: Vec<PersistedRewrittenRow>,
    edits: Vec<PersistedEdit>,
    rewrite: RewriteOptions,
    rebase: RebaseOntoOptions,
    apply: ApplyOptions,
    replay_order: Vec<String>,
    remaining: Vec<String>,
    done: Vec<String>,
    fork: String,
    onto: String,
    upstream: String,
    head_ref: String,
    orig_head: String,
    branch_tip: String,
    flatten_leaf_merges: bool,
    scoped_refs: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PersistedRewrittenRow {
    old: String,
    new: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
enum PersistedEdit {
    Reparent {
        commit: String,
        parent: String,
    },
    Reuse {
        commit: String,
        existing: String,
    },
    Drop {
        commit: String,
    },
    Rewrite {
        commit: String,
        tree: Option<String>,
        message: Option<String>,
        author: Option<String>,
    },
}

fn oid_hex(oid: &ObjectId) -> String {
    oid.to_hex()
}

fn parse_oid(hex: &str) -> Result<ObjectId, RebaseSessionError> {
    ObjectId::from_hex(hex).map_err(|e| {
        HistoryError::Planning {
            message: e.to_string(),
        }
        .into()
    })
}

fn edits_to_persist(edits: &HistoryEdits) -> Vec<PersistedEdit> {
    edits
        .iter()
        .map(|(commit, edit)| match edit {
            CommitEdit::Reparent { parent } => PersistedEdit::Reparent {
                commit: oid_hex(commit),
                parent: oid_hex(parent),
            },
            CommitEdit::Reuse(existing) => PersistedEdit::Reuse {
                commit: oid_hex(commit),
                existing: oid_hex(existing),
            },
            CommitEdit::Drop => PersistedEdit::Drop {
                commit: oid_hex(commit),
            },
            CommitEdit::Rewrite {
                tree,
                message,
                author,
                parents: _,
            } => PersistedEdit::Rewrite {
                commit: oid_hex(commit),
                tree: tree.as_ref().map(oid_hex),
                message: message.clone(),
                author: author.clone(),
            },
            CommitEdit::InsertBefore(_) | CommitEdit::SplitBefore { .. } => PersistedEdit::Drop {
                commit: oid_hex(commit),
            },
        })
        .collect()
}

fn edits_from_persist(rows: &[PersistedEdit]) -> Result<HistoryEdits, RebaseSessionError> {
    let mut edits = HistoryEdits::new();
    for row in rows {
        match row {
            PersistedEdit::Reparent { commit, parent } => {
                edits.insert(
                    parse_oid(commit)?,
                    CommitEdit::Reparent {
                        parent: parse_oid(parent)?,
                    },
                );
            }
            PersistedEdit::Reuse { commit, existing } => {
                edits.insert(parse_oid(commit)?, CommitEdit::Reuse(parse_oid(existing)?));
            }
            PersistedEdit::Drop { commit } => {
                edits.insert(parse_oid(commit)?, CommitEdit::Drop);
            }
            PersistedEdit::Rewrite {
                commit,
                tree,
                message,
                author,
            } => {
                edits.insert(
                    parse_oid(commit)?,
                    CommitEdit::Rewrite {
                        tree: tree.as_deref().map(parse_oid).transpose()?,
                        message: message.clone(),
                        author: author.clone(),
                        parents: None,
                    },
                );
            }
        }
    }
    Ok(edits)
}

fn mapping_to_persist(map: &HashMap<ObjectId, ObjectId>) -> HashMap<String, String> {
    map.iter().map(|(k, v)| (oid_hex(k), oid_hex(v))).collect()
}

fn mapping_from_persist(
    map: &HashMap<String, String>,
) -> Result<HashMap<ObjectId, ObjectId>, RebaseSessionError> {
    map.iter()
        .map(|(k, v)| Ok((parse_oid(k)?, parse_oid(v)?)))
        .collect()
}

#[allow(clippy::too_many_arguments)]
fn state_from_partial(
    partial: &super::plan::PartialRewritePlan,
    head_ref: &str,
    orig_head: ObjectId,
    request: &RebaseRequest,
    rebase: &RebaseOntoOptions,
    apply: &ApplyOptions,
    effective_onto: ObjectId,
    fork: ObjectId,
) -> PersistedRebaseState {
    let done: Vec<ObjectId> = partial
        .commits_to_replay
        .iter()
        .copied()
        .filter(|c| partial.mapping.contains_key(c))
        .collect();
    let remaining: Vec<ObjectId> = partial
        .commits_to_replay
        .iter()
        .copied()
        .filter(|c| !partial.mapping.contains_key(c))
        .collect();
    PersistedRebaseState {
        mapping: mapping_to_persist(&partial.mapping),
        rewritten: partial
            .rewritten
            .iter()
            .map(|r| PersistedRewrittenRow {
                old: oid_hex(&r.old),
                new: r.new.as_ref().map(oid_hex),
            })
            .collect(),
        edits: edits_to_persist(&partial.edits),
        rewrite: partial.opts.clone(),
        rebase: rebase.clone(),
        apply: apply.clone(),
        replay_order: partial.commits_to_replay.iter().map(oid_hex).collect(),
        remaining: remaining.iter().map(oid_hex).collect(),
        done: done.iter().map(oid_hex).collect(),
        fork: oid_hex(&fork),
        onto: oid_hex(&effective_onto),
        upstream: oid_hex(&request.upstream),
        head_ref: head_ref.to_owned(),
        orig_head: oid_hex(&orig_head),
        branch_tip: oid_hex(&request.branch_tip),
        flatten_leaf_merges: partial.flatten_leaf_merges,
        scoped_refs: partial
            .scoped_refs
            .iter()
            .map(|(n, o)| (n.clone(), oid_hex(o)))
            .collect(),
    }
}

struct LoadedState {
    mapping: HashMap<ObjectId, ObjectId>,
    rewritten: Vec<RewrittenCommit>,
    edits: HistoryEdits,
    rewrite: RewriteOptions,
    rebase: RebaseOntoOptions,
    replay_order: Vec<ObjectId>,
    remaining: Vec<ObjectId>,
    head_ref: String,
    orig_head: ObjectId,
    onto: ObjectId,
    upstream: ObjectId,
    branch_tip: ObjectId,
    flatten_leaf_merges: bool,
    scoped_refs: Vec<(String, ObjectId)>,
}

fn load_persisted_state(git_dir: &Path) -> Result<LoadedState, RebaseSessionError> {
    let path = git_dir.join("rebase-merge").join("grit-plan");
    let raw = fs::read_to_string(&path).map_err(|_| RebaseSessionError::NoRebaseInProgress)?;
    let parsed: PersistedRebaseState =
        serde_json::from_str(&raw).map_err(|_| RebaseSessionError::NoRebaseInProgress)?;
    Ok(LoadedState {
        mapping: mapping_from_persist(&parsed.mapping)?,
        rewritten: parsed
            .rewritten
            .iter()
            .map(|r| {
                Ok(RewrittenCommit {
                    old: parse_oid(&r.old)?,
                    new: r.new.as_deref().map(parse_oid).transpose()?,
                    change_id: None,
                    subject: None,
                })
            })
            .collect::<Result<_, RebaseSessionError>>()?,
        edits: edits_from_persist(&parsed.edits)?,
        rewrite: parsed.rewrite,
        rebase: parsed.rebase,
        replay_order: parsed
            .replay_order
            .iter()
            .map(|h| parse_oid(h))
            .collect::<Result<_, _>>()?,
        remaining: parsed
            .remaining
            .iter()
            .map(|h| parse_oid(h))
            .collect::<Result<_, _>>()?,
        head_ref: parsed.head_ref,
        orig_head: parse_oid(&parsed.orig_head)?,
        onto: parse_oid(&parsed.onto)?,
        upstream: parse_oid(&parsed.upstream)?,
        branch_tip: parse_oid(&parsed.branch_tip)?,
        flatten_leaf_merges: parsed.flatten_leaf_merges,
        scoped_refs: parsed
            .scoped_refs
            .iter()
            .map(|(name, hex)| parse_oid(hex).map(|oid| (name.clone(), oid)))
            .collect::<Result<_, _>>()?,
    })
}

fn write_persisted_state(
    git_dir: &Path,
    state: &PersistedRebaseState,
) -> Result<(), RebaseSessionError> {
    let dir = git_dir.join("rebase-merge");
    fs::create_dir_all(&dir).map_err(map_lib)?;
    let lock_path = dir.join("grit-plan.lock");
    let mut lock = LockFile::acquire(&lock_path, LockWait::Immediately)
        .map_err(|e| RebaseSessionError::Library(crate::error::Error::Message(e.to_string())))?;
    let body = serde_json::to_vec_pretty(state).map_err(map_json)?;
    lock.write_all(&body).map_err(map_lib)?;
    lock.commit_to(&dir.join("grit-plan"))
        .map_err(|e| RebaseSessionError::Library(crate::error::Error::Message(e.to_string())))?;
    Ok(())
}

fn install_stopped_rebase(
    repo: &Repository,
    head_ref: &str,
    orig_head: ObjectId,
    request: &RebaseRequest,
    rebase_opts: &RebaseOntoOptions,
    apply: &ApplyOptions,
    partial: super::plan::PartialRewritePlan,
) -> Result<RebaseStop, RebaseSessionError> {
    let effective_onto = request.onto.unwrap_or(request.upstream);
    let fork = super::rebase::rebase_fork(repo, request.upstream, request.branch_tip)?;
    let persisted = state_from_partial(
        &partial,
        head_ref,
        orig_head,
        request,
        rebase_opts,
        apply,
        effective_onto,
        fork,
    );
    let conflict = partial.conflict;
    let done = persisted
        .done
        .iter()
        .map(|h| parse_oid(h))
        .collect::<Result<Vec<_>, _>>()?;
    let remaining = persisted
        .remaining
        .iter()
        .map(|h| parse_oid(h))
        .collect::<Result<Vec<_>, _>>()?;
    write_git_rebase_merge_files(
        repo,
        head_ref,
        orig_head,
        effective_onto,
        &done,
        &remaining,
        &conflict.commit,
    )?;
    write_persisted_state(repo.git_dir(), &persisted)?;

    refs::write_ref(repo.git_dir(), "HEAD", &conflict.replayed_parent).map_err(map_lib)?;
    reset_index_worktree_to_head(repo, conflict.replayed_parent)?;
    let ours_tree = commit_tree(repo, conflict.replayed_parent)?;
    apply_conflict_merge_worktree(repo, &ours_tree, &conflict.merge).map_err(map_lib)?;

    let message = load_commit(repo, conflict.commit)?.message;
    fs::write(repo.git_dir().join("MERGE_MSG"), &message).map_err(map_lib)?;

    Ok(RebaseStop {
        commit: conflict.commit,
        conflicts: conflicts_from_tree_merge(&conflict.merge),
        done,
        remaining,
    })
}

fn advance_after_plan(
    repo: &Repository,
    prev: &LoadedState,
    apply: &ApplyOptions,
    outcome: ResumablePlanOutcome,
    edits: HistoryEdits,
) -> Result<RebaseOutcome, RebaseSessionError> {
    match outcome {
        ResumablePlanOutcome::Complete(mut plan) => {
            if let Some(new_tip) = plan.mapped_tip(prev.orig_head) {
                if !plan.ref_updates.iter().any(|u| u.name == prev.head_ref) {
                    plan.ref_updates.push(PlannedRefUpdate {
                        name: prev.head_ref.clone(),
                        old: prev.orig_head,
                        new: new_tip,
                    });
                }
            }
            let mut applied = apply_rewrite(repo, &plan, apply)?;
            refs::write_symbolic_ref(repo.git_dir(), "HEAD", &prev.head_ref).map_err(map_lib)?;
            let branch_tip = refs::resolve_ref(repo.git_dir(), &prev.head_ref).map_err(map_lib)?;
            reset_index_worktree_to_head(repo, branch_tip)?;
            applied.head = branch_tip;
            clear_rebase_state(repo.git_dir())?;
            Ok(RebaseOutcome::Done { applied, plan })
        }
        ResumablePlanOutcome::Partial(partial) => {
            let request = RebaseRequest {
                upstream: prev.upstream,
                onto: Some(prev.onto),
                branch_tip: prev.branch_tip,
            };
            let stop = install_stopped_rebase(
                repo,
                &prev.head_ref,
                prev.orig_head,
                &request,
                &prev.rebase,
                apply,
                *partial,
            )?;
            let _ = edits;
            Ok(RebaseOutcome::Stopped(stop))
        }
    }
}

fn write_git_rebase_merge_files(
    repo: &Repository,
    head_ref: &str,
    orig_head: ObjectId,
    onto: ObjectId,
    done: &[ObjectId],
    remaining: &[ObjectId],
    current: &ObjectId,
) -> Result<(), RebaseSessionError> {
    let dir = repo.git_dir().join("rebase-merge");
    fs::create_dir_all(&dir).map_err(map_lib)?;
    fs::write(dir.join("head-name"), format!("{head_ref}\n")).map_err(map_lib)?;
    fs::write(dir.join("onto"), format!("{}\n", onto.to_hex())).map_err(map_lib)?;
    fs::write(dir.join("orig-head"), format!("{}\n", orig_head.to_hex())).map_err(map_lib)?;
    let todo_remaining: Vec<ObjectId> = if remaining.first() == Some(current) {
        remaining.iter().skip(1).copied().collect()
    } else {
        remaining.to_vec()
    };
    fs::write(
        dir.join("git-rebase-todo"),
        todo_lines(repo, &todo_remaining)?,
    )
    .map_err(map_lib)?;
    fs::write(dir.join("done"), todo_lines(repo, done)?).map_err(map_lib)?;
    let msgnum = done.len() + 1;
    let end = done.len() + remaining.len();
    fs::write(dir.join("msgnum"), format!("{msgnum}\n")).map_err(map_lib)?;
    fs::write(dir.join("end"), format!("{end}\n")).map_err(map_lib)?;
    fs::write(dir.join("message"), load_commit(repo, *current)?.message).map_err(map_lib)?;
    fs::write(
        repo.git_dir().join("ORIG_HEAD"),
        format!("{}\n", orig_head.to_hex()),
    )
    .map_err(map_lib)?;
    Ok(())
}

fn todo_lines(repo: &Repository, commits: &[ObjectId]) -> Result<String, RebaseSessionError> {
    let mut out = String::new();
    for &oid in commits {
        let subject = super::subject::commit_message_subject_line(&load_commit(repo, oid)?.message);
        let abbrev = &oid.to_hex()[..7];
        out.push_str(&format!("pick {abbrev} {subject}\n"));
    }
    Ok(out)
}

fn rebase_session_active(git_dir: &Path) -> Result<bool, RebaseSessionError> {
    Ok(git_dir.join("rebase-merge").join("grit-plan").is_file())
}

fn clear_rebase_state(git_dir: &Path) -> Result<(), RebaseSessionError> {
    for name in ["ORIG_HEAD", "MERGE_MSG"] {
        let path = git_dir.join(name);
        if path.exists() {
            fs::remove_file(&path).map_err(map_lib)?;
        }
    }
    let dir = git_dir.join("rebase-merge");
    if dir.is_dir() {
        fs::remove_dir_all(&dir).map_err(map_lib)?;
    }
    Ok(())
}

fn branch_head(repo: &Repository) -> Result<(String, ObjectId), RebaseSessionError> {
    match resolve_head(repo.git_dir()).map_err(map_lib)? {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => Ok((refname, oid)),
        HeadState::Branch { oid: None, .. } | HeadState::Detached { .. } | HeadState::Invalid => {
            Err(RebaseSessionError::NotOnBranch)
        }
    }
}

fn resolve_head_oid(repo: &Repository) -> Result<ObjectId, RebaseSessionError> {
    resolve_head(repo.git_dir())
        .map_err(map_lib)?
        .oid()
        .copied()
        .ok_or(RebaseSessionError::NotOnBranch)
}

fn commit_tree(repo: &Repository, commit: ObjectId) -> Result<ObjectId, RebaseSessionError> {
    Ok(load_commit(repo, commit)?.tree)
}

fn load_commit(
    repo: &Repository,
    oid: ObjectId,
) -> Result<crate::objects::CommitData, RebaseSessionError> {
    let raw = repo.odb().read(&oid).map_err(map_lib)?;
    parse_commit(&raw.data).map_err(map_lib)
}

fn reset_index_worktree_to_head(
    repo: &Repository,
    head_oid: ObjectId,
) -> Result<(), RebaseSessionError> {
    let head_tree = commit_tree(repo, head_oid)?;
    let mut index = index_from_tree_with_worktree_stats(repo, &head_tree).map_err(map_lib)?;
    repo.write_index(&mut index).map_err(map_lib)?;
    checkout_between_trees(repo, None, &head_tree).map_err(map_lib)?;
    Ok(())
}

fn hard_reset_branch(
    repo: &Repository,
    refname: &str,
    target: ObjectId,
    identity: &str,
) -> Result<(), RebaseSessionError> {
    refs::write_symbolic_ref(repo.git_dir(), "HEAD", refname).map_err(map_lib)?;
    reset_to_commit(
        repo,
        &ResetOptions {
            target,
            mode: ResetMode::Hard,
            identity: identity.to_owned(),
            message: "rebase (abort)".to_owned(),
        },
    )
    .map_err(map_lib)?;
    Ok(())
}

use std::io::Write;

fn map_lib<E: Into<crate::error::Error>>(err: E) -> RebaseSessionError {
    RebaseSessionError::Library(err.into())
}

fn replan_rewrite_options(state: &LoadedState) -> RewriteOptions {
    state.rewrite.clone()
}

fn map_json(err: serde_json::Error) -> RebaseSessionError {
    RebaseSessionError::Library(crate::error::Error::Message(err.to_string()))
}
