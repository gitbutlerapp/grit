//! Multi-commit cherry-pick and revert sequences with Git-compatible on-disk state.
//!
//! [`start_replay_sequence`] applies an ordered list of commits onto the current branch,
//! writing `.git/sequencer/`, `CHERRY_PICK_HEAD` / `REVERT_HEAD`, `MERGE_MSG`, and `ORIG_HEAD`
//! when a step stops for conflicts or an empty commit. [`continue_replay_sequence`],
//! [`skip_replay_step`], and [`abort_replay_sequence`] resume or cancel an in-progress sequence.
//!
//! Single-commit callers should keep using [`crate::porcelain::replay::replay_commit`].

use std::fs;
use std::num::NonZeroU32;
use std::path::Path;

use crate::change_id::{change_id, ChangeId, ChangeIdPolicy};
use crate::commit_trailers::finalize_cherry_pick_message;
use crate::error::{Error, Result};
use crate::merge_file::{ConflictStyle, MergeFavor};
use crate::merge_trees::{
    merge_trees_three_way, TreeMergeConflictPresentation, WhitespaceMergeOptions,
};
use crate::objects::{parse_commit, CommitData, ObjectId, ObjectKind};
use crate::operation::Conflict;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::commit::{stamp_change_id_on_new_commit, write_commit_object};
use crate::porcelain::index_from_tree::index_from_tree_with_worktree_stats;
use crate::porcelain::merge::apply_conflict_merge_worktree;
use crate::porcelain::replay::ReplayDirection;
use crate::porcelain::reset::{reset_to_commit, ResetMode, ResetOptions};
use crate::porcelain::revert::merge_commit_message_for_revert;
use crate::porcelain::tree_merge_checkout::apply_clean_tree_merge_to_worktree;
use crate::porcelain::worktree_guard::ensure_worktree_clean_for_pick;
use crate::refs::{update_branch_for_commit_with_config, BranchCommitRefUpdate};
use crate::repo::Repository;
use crate::rev_list::{rev_list, OrderingMode, RevListOptions};
use crate::rev_parse::{resolve_revision_as_commit, try_parse_double_dot_log_range};
use crate::state::{
    read_cherry_pick_head, read_merge_msg, read_orig_head, read_revert_head, resolve_head,
    HeadState,
};
use crate::stripspace::{process as stripspace_process, Mode as StripspaceMode};
use crate::write_tree::{write_tree_update_index, WriteTreeFlags};

/// How to handle a replay step whose merged tree matches `HEAD` (empty patch).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ReplayEmptyPolicy {
    /// Stop the sequence and leave sequencer state for `--continue` / `--skip`.
    #[default]
    Stop,
    /// Omit the commit and apply the next todo entry.
    Drop,
    /// Record an empty commit for the step.
    Keep,
}

/// Per-sequence merge and message options (stored under `.git/sequencer/opts` for grit starts).
#[derive(Debug, Clone)]
pub struct ReplayOptions {
    /// Parent number (1-based) when replaying merge commits (`-m`).
    pub mainline: Option<NonZeroU32>,
    /// Append `(cherry picked from commit …)` (`-x`).
    pub record_origin: bool,
    /// Apply merges to the index only (`-n` / `--no-commit`).
    pub no_commit: bool,
    /// Behavior when the step would not change `HEAD`'s tree.
    pub empty: ReplayEmptyPolicy,
    /// Replace the default message for each step.
    pub message_override: Option<String>,
    /// Textual conflict resolution favor (`-Xours` / `-Xtheirs`).
    pub favor: MergeFavor,
    /// Conflict marker style (`merge` vs `diff3`).
    pub conflict_style: ConflictStyle,
}

impl Default for ReplayOptions {
    fn default() -> Self {
        Self {
            mainline: None,
            record_origin: false,
            no_commit: false,
            empty: ReplayEmptyPolicy::Stop,
            message_override: None,
            favor: MergeFavor::default(),
            conflict_style: ConflictStyle::default(),
        }
    }
}

/// Inputs for [`start_replay_sequence`].
#[derive(Debug, Clone)]
pub struct ReplaySequenceRequest {
    /// Commits to replay, in apply order.
    pub commits: Vec<ObjectId>,
    /// Pick or revert semantics for every commit in `commits`.
    pub direction: ReplayDirection,
    /// Committer identity in Git header form: `Name <email> <unix-time> <tz>`.
    pub committer: String,
    /// Merge, empty, and message options for the whole sequence.
    pub options: ReplayOptions,
}

/// One commit produced by a replay step (pick or revert).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReplayCreated {
    /// Commit that was replayed.
    pub source: ObjectId,
    /// New commit written onto the current branch.
    pub new_oid: ObjectId,
    /// Stored `change-id` on `new_oid` when present.
    pub change_id: Option<ChangeId>,
}

/// Result of starting or advancing a replay sequence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SequenceOutcome {
    /// Every requested step finished (commits may have been dropped per `empty`).
    Completed {
        /// New commits created during this call (not prior `created_so_far`).
        created: Vec<ReplayCreated>,
        /// Source commits skipped as empty/redundant.
        dropped: Vec<ObjectId>,
    },
    /// The current step stopped on merge conflicts.
    Conflicts {
        /// Source commit being replayed.
        current: ObjectId,
        /// Conflicted paths and kinds.
        conflicts: Vec<Conflict>,
        /// Commits created before this stop in the sequence.
        created_so_far: Vec<ReplayCreated>,
        /// Remaining todo oids (including `current`).
        remaining: Vec<ObjectId>,
    },
    /// The current step is empty relative to `HEAD` and `empty` is [`ReplayEmptyPolicy::Stop`].
    Empty {
        /// Source commit that would be empty.
        current: ObjectId,
        /// Commits created before this stop.
        created_so_far: Vec<ReplayCreated>,
        /// Remaining todo oids (including `current`).
        remaining: Vec<ObjectId>,
    },
}

/// Expand revision arguments for a pick/revert sequence into commit OIDs, oldest first.
///
/// Each entry in `specs` is either a single commit-ish (`branch`, `HEAD~2`, full oid) or a
/// two-dot range `A..B` / `..B` / `A..` (commits reachable from the right tip but not the left,
/// in topological order from oldest to newest). Symmetric `A...B` is not supported here.
///
/// # Errors
///
/// Propagates rev-parse and rev-list errors. Merge commits are not rejected here; replay stops
/// with [`Error::MergeCommit`] when a step is applied without `mainline`.
pub fn commits_to_replay(repo: &Repository, specs: &[String]) -> Result<Vec<ObjectId>> {
    let mut out = Vec::new();
    for spec in specs {
        if let Some((exclude, include)) = try_parse_double_dot_log_range(repo, spec)? {
            let positive = vec![include.to_hex()];
            let negative = vec![exclude.to_hex()];
            let walk_opts = RevListOptions {
                ordering: OrderingMode::Topo,
                reverse: false,
                use_commit_graph: true,
                full_history: true,
                ..RevListOptions::default()
            };
            let walk = rev_list(repo, &positive, &negative, &walk_opts)?;
            let mut commits = walk.commits;
            commits.retain(|oid| *oid != exclude);
            if include != exclude && !commits.contains(&include) {
                commits.push(include);
            }
            commits.reverse();
            out.extend(commits);
        } else {
            out.push(resolve_revision_as_commit(repo, spec)?);
        }
    }
    Ok(out)
}

/// Apply `req.commits` onto the current branch, stopping on conflict or empty per `req.options`.
///
/// # Errors
///
/// - [`Error::DetachedHead`], [`Error::UnbornHead`], [`Error::MergeCommit`] (without `mainline`).
/// - [`Error::ReplaySourceAtHead`] when picking a commit already at `HEAD`.
/// - Worktree cleanliness, merge, checkout, and ref update failures.
pub fn start_replay_sequence(
    repo: &Repository,
    req: &ReplaySequenceRequest,
) -> Result<SequenceOutcome> {
    if replay_in_progress(repo.git_dir())? {
        return Err(Error::Message(
            "cherry-pick or revert is already in progress".into(),
        ));
    }
    ensure_worktree_clean_for_pick(repo)?;
    let (refname, head_oid) = branch_head(repo)?;
    if req.commits.is_empty() {
        return Ok(SequenceOutcome::Completed {
            created: Vec::new(),
            dropped: Vec::new(),
        });
    }
    if req.commits.len() > 1 && req.options.message_override.is_some() {
        return Err(Error::Message(
            "message_override is only valid when replaying a single commit".into(),
        ));
    }
    let mut todo = build_todo_from_commits(repo, req.direction, &req.commits)?;
    write_replay_sequence_start(repo, head_oid, req)?;
    run_sequence(
        repo,
        &refname,
        head_oid,
        head_oid,
        head_oid,
        req.direction,
        &req.committer,
        &req.options,
        &mut todo,
        Vec::new(),
        Vec::new(),
    )
}

fn sanitize_replay_continue_message(repo: &Repository, raw: &str) -> Result<String> {
    let config = repo.config()?;
    let comment_char = config
        .get("core.commentChar")
        .and_then(|v| v.chars().next())
        .unwrap_or('#');
    let processed = stripspace_process(
        raw.as_bytes(),
        &StripspaceMode::StripComments(comment_char.to_string()),
    );
    let message = String::from_utf8(processed).map_err(|_| Error::NothingToCommit)?;
    if message.trim().is_empty() {
        return Err(Error::NothingToCommit);
    }
    let mut message = message;
    if !message.ends_with('\n') {
        message.push('\n');
    }
    Ok(message)
}

/// Commit the resolved index for the current step and apply subsequent todo entries.
///
/// # Errors
///
/// - [`Error::NoReplayInProgress`] when no sequence is active.
/// - [`Error::ReplayContinueUnmerged`] when the index still has unmerged entries.
pub fn continue_replay_sequence(repo: &Repository, identity: &str) -> Result<SequenceOutcome> {
    let active = load_active_replay(repo)?;
    let mut index = repo.load_index()?;
    if index.has_unmerged_entries() {
        return Err(Error::ReplayContinueUnmerged);
    }
    let (refname, head_oid) = branch_head(repo)?;
    let source = active.current_source()?.ok_or(Error::NoReplayInProgress)?;
    let raw_message = read_merge_msg(repo.git_dir())?.ok_or(Error::NoReplayInProgress)?;
    let message = sanitize_replay_continue_message(repo, &raw_message)?;
    let new_tree = write_tree_update_index(
        repo.odb(),
        &mut index,
        "",
        crate::write_tree::WriteTreeFlags::silent(),
    )?;
    repo.write_index(&mut index)?;
    let head_tree = commit_tree(repo, &head_oid)?;
    if active.opts.no_commit {
        if new_tree == head_tree {
            return Err(Error::NothingToCommit);
        }
    } else if new_tree == head_tree {
        match active.opts.empty {
            ReplayEmptyPolicy::Keep => {}
            ReplayEmptyPolicy::Stop => return Err(Error::NothingToCommit),
            ReplayEmptyPolicy::Drop => {
                clear_step_heads(repo.git_dir(), active.direction)?;
                let mut todo = active.todo;
                if !todo.is_empty() {
                    todo.remove(0);
                }
                write_sequencer_todo(repo.git_dir(), &todo)?;
                if todo.is_empty() {
                    clear_replay_state(repo.git_dir())?;
                    return Ok(SequenceOutcome::Completed {
                        created: Vec::new(),
                        dropped: vec![source],
                    });
                }
                let sequence_start =
                    read_sequencer_head(repo.git_dir()).unwrap_or(active.abort_safety);
                return run_sequence(
                    repo,
                    &refname,
                    head_oid,
                    head_oid,
                    sequence_start,
                    active.direction,
                    identity,
                    &active.opts,
                    &mut todo,
                    Vec::new(),
                    vec![source],
                );
            }
        }
    }

    if active.opts.no_commit {
        clear_step_heads(repo.git_dir(), active.direction)?;
        let mut todo = active.todo;
        if !todo.is_empty() {
            todo.remove(0);
        }
        write_sequencer_todo(repo.git_dir(), &todo)?;
        write_sequencer_abort_safety(repo.git_dir(), head_oid)?;
        if todo.is_empty() {
            clear_replay_state(repo.git_dir())?;
            return Ok(SequenceOutcome::Completed {
                created: Vec::new(),
                dropped: Vec::new(),
            });
        }
        let sequence_start = read_sequencer_head(repo.git_dir()).unwrap_or(active.abort_safety);
        return run_sequence(
            repo,
            &refname,
            head_oid,
            head_oid,
            sequence_start,
            active.direction,
            identity,
            &active.opts,
            &mut todo,
            Vec::new(),
            Vec::new(),
        );
    }
    let allow_empty = active.opts.empty == ReplayEmptyPolicy::Keep;
    let new_oid = write_replay_commit(
        repo,
        &refname,
        head_oid,
        identity,
        &message,
        &source,
        active.direction,
        new_tree,
        allow_empty,
    )?;
    clear_step_heads(repo.git_dir(), active.direction)?;
    let mut todo = active.todo;
    if !todo.is_empty() {
        todo.remove(0);
    }
    write_sequencer_todo(repo.git_dir(), &todo)?;
    write_sequencer_abort_safety(repo.git_dir(), new_oid)?;
    if todo.is_empty() {
        clear_replay_state(repo.git_dir())?;
        let created = replay_created_record(repo, source, new_oid)?;
        return Ok(SequenceOutcome::Completed {
            created: vec![created],
            dropped: Vec::new(),
        });
    }
    let sequence_start = read_sequencer_head(repo.git_dir()).unwrap_or(active.abort_safety);
    let continued = replay_created_record(repo, source, new_oid)?;
    match run_sequence(
        repo,
        &refname,
        new_oid,
        new_oid,
        sequence_start,
        active.direction,
        identity,
        &active.opts,
        &mut todo,
        vec![continued],
        Vec::new(),
    )? {
        SequenceOutcome::Completed { created, dropped } => {
            Ok(SequenceOutcome::Completed { created, dropped })
        }
        other => Ok(other),
    }
}

/// Skip the current todo step and apply later entries.
///
/// Resets the index and worktree to `HEAD`, then runs the remaining todo list.
///
/// # Errors
///
/// - [`Error::NoReplayInProgress`] when no sequence is active.
pub fn skip_replay_step(repo: &Repository, identity: &str) -> Result<SequenceOutcome> {
    let active = load_active_replay(repo)?;
    let (refname, head_oid) = branch_head(repo)?;
    reset_index_worktree_to_head(repo, &head_oid)?;
    clear_step_heads(repo.git_dir(), active.direction)?;
    let mut todo = active.todo;
    let dropped = if todo.is_empty() {
        None
    } else {
        Some(todo.remove(0).oid)
    };
    write_sequencer_todo(repo.git_dir(), &todo)?;
    if todo.is_empty() {
        clear_replay_state(repo.git_dir())?;
        return Ok(SequenceOutcome::Completed {
            created: Vec::new(),
            dropped: dropped.into_iter().collect(),
        });
    }
    let dropped: Vec<ObjectId> = dropped.into_iter().collect();
    let sequence_start = read_sequencer_head(repo.git_dir()).unwrap_or(active.abort_safety);
    match run_sequence(
        repo,
        &refname,
        head_oid,
        head_oid,
        sequence_start,
        active.direction,
        identity,
        &active.opts,
        &mut todo,
        Vec::new(),
        dropped,
    )? {
        SequenceOutcome::Completed { created, dropped } => {
            Ok(SequenceOutcome::Completed { created, dropped })
        }
        other => Ok(other),
    }
}

/// Cancel an in-progress replay, restoring branch, index, and worktree to the saved head.
///
/// # Errors
///
/// - [`Error::NoReplayInProgress`] when nothing is in progress.
/// - [`Error::ReplayAbortHeadMoved`] when `HEAD` is no longer at `sequencer/abort-safety`.
pub fn abort_replay_sequence(repo: &Repository) -> Result<()> {
    let active = load_active_replay(repo)?;
    let (_, head_oid) = branch_head(repo)?;
    if head_oid != active.abort_safety {
        return Err(Error::ReplayAbortHeadMoved);
    }
    let restore_oid = read_sequencer_head(repo.git_dir())
        .or(read_orig_head(repo.git_dir()).ok().flatten())
        .unwrap_or(active.abort_safety);
    let (refname, _) = branch_head(repo)?;
    hard_reset_branch(repo, &refname, restore_oid)?;
    clear_replay_state(repo.git_dir())?;
    Ok(())
}

// --- internal ---

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TodoVerb {
    Pick,
    Revert,
}

#[derive(Debug, Clone)]
struct TodoEntry {
    verb: TodoVerb,
    oid: ObjectId,
    subject: String,
}

#[derive(Debug, Clone)]
struct ActiveReplay {
    direction: ReplayDirection,
    opts: ReplayOptions,
    todo: Vec<TodoEntry>,
    abort_safety: ObjectId,
}

fn replay_in_progress(git_dir: &Path) -> Result<bool> {
    Ok(read_cherry_pick_head(git_dir)?.is_some()
        || read_revert_head(git_dir)?.is_some()
        || git_dir.join("sequencer").join("todo").is_file())
}

fn branch_head(repo: &Repository) -> Result<(String, ObjectId)> {
    match resolve_head(repo.git_dir())? {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => Ok((refname, oid)),
        HeadState::Branch { .. } => Err(Error::UnbornHead),
        HeadState::Detached { .. } => Err(Error::DetachedHead),
        HeadState::Invalid => Err(Error::Message("HEAD is in an unknown state".into())),
    }
}

fn build_todo_from_commits(
    repo: &Repository,
    direction: ReplayDirection,
    commits: &[ObjectId],
) -> Result<Vec<TodoEntry>> {
    let verb = match direction {
        ReplayDirection::Pick => TodoVerb::Pick,
        ReplayDirection::Revert => TodoVerb::Revert,
    };
    commits
        .iter()
        .map(|oid| {
            let subject = commit_subject(repo, *oid)?;
            Ok(TodoEntry {
                verb,
                oid: *oid,
                subject,
            })
        })
        .collect()
}

fn commit_subject(repo: &Repository, oid: ObjectId) -> Result<String> {
    let obj = repo.odb().read(&oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(commit
        .message
        .lines()
        .next()
        .unwrap_or("")
        .trim()
        .to_owned())
}

#[allow(clippy::too_many_arguments)]
fn run_sequence(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    abort_safety: ObjectId,
    sequence_start: ObjectId,
    direction: ReplayDirection,
    committer: &str,
    opts: &ReplayOptions,
    todo: &mut Vec<TodoEntry>,
    prior_created: Vec<ReplayCreated>,
    mut dropped: Vec<ObjectId>,
) -> Result<SequenceOutcome> {
    let mut head = head_oid;
    let mut created_this_call = Vec::new();
    let _ = abort_safety;
    write_sequencer_abort_safety(repo.git_dir(), head_oid)?;
    while let Some(entry) = todo.first().cloned() {
        match apply_one_step(
            repo,
            refname,
            &mut head,
            sequence_start,
            direction,
            committer,
            opts,
            &entry,
        )? {
            StepResult::Committed { oid } => {
                todo.remove(0);
                let record = replay_created_record(repo, entry.oid, oid)?;
                created_this_call.push(record);
                write_sequencer_abort_safety(repo.git_dir(), oid)?;
                write_sequencer_todo(repo.git_dir(), todo)?;
                if todo.is_empty() {
                    clear_replay_state(repo.git_dir())?;
                    return Ok(SequenceOutcome::Completed {
                        created: merge_created_records(&prior_created, &created_this_call),
                        dropped,
                    });
                }
                write_sequencer_abort_safety(repo.git_dir(), head)?;
            }
            StepResult::AppliedNoCommit => {
                todo.remove(0);
                write_sequencer_todo(repo.git_dir(), todo)?;
                write_sequencer_abort_safety(repo.git_dir(), head)?;
                if todo.is_empty() {
                    clear_replay_state(repo.git_dir())?;
                    return Ok(SequenceOutcome::Completed {
                        created: merge_created_records(&prior_created, &created_this_call),
                        dropped,
                    });
                }
                write_sequencer_abort_safety(repo.git_dir(), head)?;
            }
            StepResult::DroppedEmpty => {
                todo.remove(0);
                dropped.push(entry.oid);
                write_sequencer_todo(repo.git_dir(), todo)?;
                continue;
            }
            StepResult::Conflicts { conflicts } => {
                let remaining: Vec<ObjectId> = todo.iter().map(|t| t.oid).collect();
                write_sequencer_state(
                    repo,
                    sequence_start,
                    head,
                    todo,
                    opts,
                    direction,
                    committer,
                )?;
                record_replay_stop(
                    repo,
                    direction,
                    sequence_start,
                    entry.oid,
                    &entry,
                    opts,
                    committer,
                )?;
                return Ok(SequenceOutcome::Conflicts {
                    current: entry.oid,
                    conflicts,
                    created_so_far: merge_created_records(&prior_created, &created_this_call),
                    remaining,
                });
            }
            StepResult::EmptyStop => {
                let remaining: Vec<ObjectId> = todo.iter().map(|t| t.oid).collect();
                write_sequencer_state(
                    repo,
                    sequence_start,
                    head,
                    todo,
                    opts,
                    direction,
                    committer,
                )?;
                record_replay_stop(
                    repo,
                    direction,
                    sequence_start,
                    entry.oid,
                    &entry,
                    opts,
                    committer,
                )?;
                return Ok(SequenceOutcome::Empty {
                    current: entry.oid,
                    created_so_far: merge_created_records(&prior_created, &created_this_call),
                    remaining,
                });
            }
        }
    }
    clear_replay_state(repo.git_dir())?;
    Ok(SequenceOutcome::Completed {
        created: merge_created_records(&prior_created, &created_this_call),
        dropped,
    })
}

fn merge_created_records(
    started: &[ReplayCreated],
    this_call: &[ReplayCreated],
) -> Vec<ReplayCreated> {
    let mut out = started.to_vec();
    out.extend_from_slice(this_call);
    out
}

enum StepResult {
    Committed { oid: ObjectId },
    AppliedNoCommit,
    DroppedEmpty,
    Conflicts { conflicts: Vec<Conflict> },
    EmptyStop,
}

#[allow(clippy::too_many_arguments)]
fn apply_one_step(
    repo: &Repository,
    refname: &str,
    head: &mut ObjectId,
    _sequence_start: ObjectId,
    direction: ReplayDirection,
    committer: &str,
    opts: &ReplayOptions,
    entry: &TodoEntry,
) -> Result<StepResult> {
    let source_oid = entry.oid;
    if direction == ReplayDirection::Pick && source_oid == *head {
        return Err(Error::ReplaySourceAtHead);
    }
    let source_obj = repo.odb().read(&source_oid)?;
    let source = parse_commit(&source_obj.data)?;
    let head_tree = commit_tree(repo, head)?;
    let ours_tree = if opts.no_commit {
        index_stage0_tree(repo)?
    } else {
        head_tree
    };
    let empty_compare_tree = if opts.no_commit { ours_tree } else { head_tree };
    let (base_tree, ours_tree, theirs_tree) = merge_trees_for_replay(
        repo,
        direction,
        &source,
        ours_tree,
        opts.mainline,
        source_oid,
    )?;

    if direction == ReplayDirection::Pick && base_tree == source.tree {
        return match opts.empty {
            ReplayEmptyPolicy::Stop => Ok(StepResult::EmptyStop),
            ReplayEmptyPolicy::Drop => Ok(StepResult::DroppedEmpty),
            ReplayEmptyPolicy::Keep if opts.no_commit => Ok(StepResult::EmptyStop),
            ReplayEmptyPolicy::Keep => {
                let message =
                    replay_message(repo, opts, direction, &source, source_oid, committer)?;
                let new_oid = write_replay_commit(
                    repo,
                    refname,
                    *head,
                    committer,
                    &message,
                    &source_oid,
                    direction,
                    head_tree,
                    true,
                )?;
                *head = new_oid;
                Ok(StepResult::Committed { oid: new_oid })
            }
        };
    }

    let presentation = TreeMergeConflictPresentation {
        style: opts.conflict_style,
        df_directory_ours_commit: Some(*head),
        df_directory_theirs_commit: Some(source_oid),
        ..TreeMergeConflictPresentation::default()
    };
    let merged = merge_trees_three_way(
        repo,
        base_tree,
        ours_tree,
        theirs_tree,
        opts.favor,
        WhitespaceMergeOptions::default(),
        None,
        presentation,
    )?;

    if crate::operation::tree_merge_has_conflicts(&merged) {
        apply_conflict_merge_worktree(repo, &ours_tree, &merged)?;
        return Ok(StepResult::Conflicts {
            conflicts: crate::operation::conflicts_from_tree_merge(&merged),
        });
    }

    let new_tree = merged.result_tree;
    if new_tree == empty_compare_tree {
        return match opts.empty {
            ReplayEmptyPolicy::Stop => Ok(StepResult::EmptyStop),
            ReplayEmptyPolicy::Drop => Ok(StepResult::DroppedEmpty),
            ReplayEmptyPolicy::Keep if opts.no_commit => Ok(StepResult::EmptyStop),
            ReplayEmptyPolicy::Keep => {
                let message =
                    replay_message(repo, opts, direction, &source, source_oid, committer)?;
                let new_oid = write_replay_commit(
                    repo,
                    refname,
                    *head,
                    committer,
                    &message,
                    &source_oid,
                    direction,
                    empty_compare_tree,
                    true,
                )?;
                *head = new_oid;
                Ok(StepResult::Committed { oid: new_oid })
            }
        };
    }

    if opts.no_commit {
        apply_clean_tree_merge_to_worktree(repo, &ours_tree, &merged)?;
        return Ok(StepResult::AppliedNoCommit);
    }

    let message = replay_message(repo, opts, direction, &source, source_oid, committer)?;
    apply_clean_tree_merge_to_worktree(repo, &head_tree, &merged)?;
    let new_oid = write_replay_commit(
        repo,
        refname,
        *head,
        committer,
        &message,
        &source_oid,
        direction,
        new_tree,
        false,
    )?;
    *head = new_oid;
    Ok(StepResult::Committed { oid: new_oid })
}

fn merge_trees_for_replay(
    repo: &Repository,
    direction: ReplayDirection,
    source: &CommitData,
    head_tree: ObjectId,
    mainline: Option<NonZeroU32>,
    source_oid: ObjectId,
) -> Result<(ObjectId, ObjectId, ObjectId)> {
    let parent_tree = |parent: &ObjectId| commit_tree(repo, parent);
    let empty = || repo.odb().write(ObjectKind::Tree, &[]);
    match direction {
        ReplayDirection::Pick => {
            let base = if source.parents.is_empty() {
                empty()?
            } else if source.parents.len() == 1 {
                parent_tree(&source.parents[0])?
            } else {
                let m = mainline.ok_or(Error::MergeCommit { oid: source_oid })?;
                let idx = m.get() as usize - 1;
                let parent = source
                    .parents
                    .get(idx)
                    .ok_or(Error::MergeCommit { oid: source_oid })?;
                parent_tree(parent)?
            };
            Ok((base, head_tree, source.tree))
        }
        ReplayDirection::Revert => {
            if source.parents.is_empty() {
                return Ok((source.tree, head_tree, empty()?));
            }
            if source.parents.len() == 1 {
                let ptree = parent_tree(&source.parents[0])?;
                return Ok((source.tree, head_tree, ptree));
            }
            let m = mainline.ok_or(Error::MergeCommit { oid: source_oid })?;
            let idx = m.get() as usize - 1;
            let parent = source
                .parents
                .get(idx)
                .copied()
                .ok_or(Error::MergeCommit { oid: source_oid })?;
            let base = parent_tree(&parent)?;
            Ok((source.tree, head_tree, base))
        }
    }
}

fn replay_message(
    repo: &Repository,
    opts: &ReplayOptions,
    direction: ReplayDirection,
    source: &CommitData,
    source_oid: ObjectId,
    committer: &str,
) -> Result<String> {
    if let Some(msg) = &opts.message_override {
        let mut message = msg.trim().to_owned();
        if !message.is_empty() && !message.ends_with('\n') {
            message.push('\n');
        }
        return Ok(message);
    }
    let config = repo.config()?;
    match direction {
        ReplayDirection::Pick => {
            let (name, email) = identity_name_email(committer);
            Ok(finalize_cherry_pick_message(
                &source.message,
                opts.record_origin,
                false,
                name,
                email,
                config.as_ref(),
                &source_oid.to_hex(),
            ))
        }
        ReplayDirection::Revert => {
            let mainline_parent = (source.parents.len() > 1)
                .then(|| {
                    opts.mainline.and_then(|m| {
                        let idx = m.get() as usize - 1;
                        source.parents.get(idx).copied()
                    })
                })
                .flatten();
            let (title, body) =
                merge_commit_message_for_revert(source, source_oid, false, '#', mainline_parent);
            Ok(format!("{title}\n{body}"))
        }
    }
}

fn replay_created_record(
    repo: &Repository,
    source: ObjectId,
    new_oid: ObjectId,
) -> Result<ReplayCreated> {
    let obj = repo.odb().read(&new_oid)?;
    let commit = parse_commit(&obj.data)?;
    Ok(ReplayCreated {
        source,
        new_oid,
        change_id: change_id(&commit),
    })
}

fn identity_name_email(ident: &str) -> (&str, &str) {
    let start = ident.find('<').map(|i| i + 1);
    let end = ident.find('>');
    match (start, end) {
        (Some(s), Some(e)) if e > s => {
            let name = ident[..ident.find('<').unwrap_or(0)].trim();
            let email = &ident[s..e];
            (name, email)
        }
        _ => ("", ""),
    }
}

#[allow(clippy::too_many_arguments)]
fn write_replay_commit(
    repo: &Repository,
    refname: &str,
    head_oid: ObjectId,
    committer: &str,
    message: &str,
    _source: &ObjectId,
    direction: ReplayDirection,
    tree: ObjectId,
    allow_empty: bool,
) -> Result<ObjectId> {
    let head_tree = commit_tree(repo, &head_oid)?;
    if !allow_empty && tree == head_tree {
        return Err(Error::NothingToCommit);
    }
    let source_obj = repo.odb().read(_source)?;
    let source = parse_commit(&source_obj.data)?;
    let author = match direction {
        ReplayDirection::Pick => source.author.clone(),
        ReplayDirection::Revert => committer.to_owned(),
    };
    let mut commit_data = CommitData {
        tree,
        parents: vec![head_oid],
        author,
        committer: committer.to_owned(),
        author_raw: Vec::new(),
        committer_raw: Vec::new(),
        encoding: None,
        message: message.to_owned(),
        raw_message: None,
        extra_headers: Vec::new(),
    };
    let config = repo.config()?;
    let policy = ChangeIdPolicy::from_config(config.as_ref());
    stamp_change_id_on_new_commit(repo, &policy, &mut commit_data)?;
    let new_oid = write_commit_object(repo, &commit_data, None)?;
    let subject = commit_data.message.lines().next().unwrap_or("").trim_end();
    let reflog_msg = match direction {
        ReplayDirection::Pick => format!("cherry-pick: {subject}"),
        ReplayDirection::Revert => format!("revert: {subject}"),
    };
    let ref_update = BranchCommitRefUpdate {
        branch_ref: refname,
        expected_old: Some(head_oid),
        new_oid,
        identity: committer,
        reflog_message: &reflog_msg,
    };
    update_branch_for_commit_with_config(repo.git_dir(), &ref_update, config.as_ref())?;
    Ok(new_oid)
}

#[allow(clippy::too_many_arguments)]
fn record_replay_stop(
    repo: &Repository,
    direction: ReplayDirection,
    orig_head: ObjectId,
    source_oid: ObjectId,
    entry: &TodoEntry,
    opts: &ReplayOptions,
    committer: &str,
) -> Result<()> {
    let git_dir = repo.git_dir();
    let source_obj = repo.odb().read(&source_oid)?;
    let source = parse_commit(&source_obj.data)?;
    let message = replay_message(repo, opts, direction, &source, source_oid, committer)?;
    fs::write(
        git_dir.join("ORIG_HEAD"),
        format!("{}\n", orig_head.to_hex()),
    )
    .map_err(Error::Io)?;
    fs::write(git_dir.join("MERGE_MSG"), &message).map_err(Error::Io)?;
    let head_name = match direction {
        ReplayDirection::Pick => "CHERRY_PICK_HEAD",
        ReplayDirection::Revert => "REVERT_HEAD",
    };
    fs::write(
        git_dir.join(head_name),
        format!("{}\n", source_oid.to_hex()),
    )
    .map_err(Error::Io)?;
    let _ = entry;
    Ok(())
}

fn write_replay_sequence_start(
    repo: &Repository,
    sequence_start: ObjectId,
    req: &ReplaySequenceRequest,
) -> Result<()> {
    let git_dir = repo.git_dir();
    fs::create_dir_all(git_dir.join("sequencer")).map_err(Error::Io)?;
    fs::write(
        git_dir.join("sequencer").join("head"),
        format!("{}\n", sequence_start.to_hex()),
    )
    .map_err(Error::Io)?;
    write_sequencer_abort_safety(git_dir, sequence_start)?;
    write_persisted_options(git_dir, req)?;
    Ok(())
}

fn write_sequencer_abort_safety(git_dir: &Path, head: ObjectId) -> Result<()> {
    fs::create_dir_all(git_dir.join("sequencer")).map_err(Error::Io)?;
    fs::write(
        git_dir.join("sequencer").join("abort-safety"),
        format!("{}\n", head.to_hex()),
    )
    .map_err(Error::Io)?;
    Ok(())
}

fn write_sequencer_state(
    repo: &Repository,
    sequence_start: ObjectId,
    current_head: ObjectId,
    todo: &[TodoEntry],
    opts: &ReplayOptions,
    direction: ReplayDirection,
    committer: &str,
) -> Result<()> {
    let git_dir = repo.git_dir();
    fs::create_dir_all(git_dir.join("sequencer")).map_err(Error::Io)?;
    fs::write(
        git_dir.join("sequencer").join("head"),
        format!("{}\n", sequence_start.to_hex()),
    )
    .map_err(Error::Io)?;
    fs::write(
        git_dir.join("sequencer").join("abort-safety"),
        format!("{}\n", current_head.to_hex()),
    )
    .map_err(Error::Io)?;
    write_sequencer_todo(git_dir, todo)?;
    write_persisted_options(
        git_dir,
        &ReplaySequenceRequest {
            commits: Vec::new(),
            direction,
            committer: committer.to_owned(),
            options: opts.clone(),
        },
    )
}

fn write_sequencer_todo(git_dir: &Path, todo: &[TodoEntry]) -> Result<()> {
    if todo.is_empty() {
        return Ok(());
    }
    fs::create_dir_all(git_dir.join("sequencer")).map_err(Error::Io)?;
    let mut lines = String::new();
    for entry in todo {
        let cmd = match entry.verb {
            TodoVerb::Pick => "pick",
            TodoVerb::Revert => "revert",
        };
        let abbrev = abbrev_oid(entry.oid);
        lines.push_str(&format!("{cmd} {abbrev} {}\n", entry.subject));
    }
    fs::write(git_dir.join("sequencer").join("todo"), lines).map_err(Error::Io)?;
    Ok(())
}

fn abbrev_oid(oid: ObjectId) -> String {
    oid.to_hex()[..7].to_owned()
}

fn write_persisted_options(git_dir: &Path, req: &ReplaySequenceRequest) -> Result<()> {
    fs::create_dir_all(git_dir.join("sequencer")).map_err(Error::Io)?;
    let mut git_opts = String::new();
    if req.options.record_origin {
        git_opts.push_str("[options]\n\trecord-origin = true\n");
        fs::write(git_dir.join("sequencer").join("opts"), git_opts).map_err(Error::Io)?;
    }
    let empty = match req.options.empty {
        ReplayEmptyPolicy::Stop => "stop",
        ReplayEmptyPolicy::Drop => "drop",
        ReplayEmptyPolicy::Keep => "keep",
    };
    let direction = match req.direction {
        ReplayDirection::Pick => "pick",
        ReplayDirection::Revert => "revert",
    };
    let favor = match req.options.favor {
        MergeFavor::None => "none",
        MergeFavor::Ours => "ours",
        MergeFavor::Theirs => "theirs",
        MergeFavor::Union => "union",
    };
    let style = match req.options.conflict_style {
        ConflictStyle::Merge => "merge",
        ConflictStyle::Diff3 => "diff3",
        ConflictStyle::ZealousDiff3 => "zealous-diff3",
    };
    let mainline = req
        .options
        .mainline
        .map(|m| m.get().to_string())
        .unwrap_or_default();
    let mut body = String::new();
    body.push_str(&format!("direction = {direction}\n"));
    body.push_str(&format!("record-origin = {}\n", req.options.record_origin));
    body.push_str(&format!("no-commit = {}\n", req.options.no_commit));
    body.push_str(&format!("empty = {empty}\n"));
    body.push_str(&format!("favor = {favor}\n"));
    body.push_str(&format!("conflict-style = {style}\n"));
    body.push_str(&format!("mainline = {mainline}\n"));
    body.push_str(&format!("committer = {}\n", req.committer));
    if let Some(msg) = &req.options.message_override {
        body.push_str(&format!("message-override = {msg}\n"));
    }
    fs::write(git_dir.join("sequencer").join("grit-opts"), body).map_err(Error::Io)?;
    Ok(())
}

fn load_active_replay(repo: &Repository) -> Result<ActiveReplay> {
    let git_dir = repo.git_dir();
    let todo_path = git_dir.join("sequencer").join("todo");
    let todo = if todo_path.is_file() {
        parse_sequencer_todo(repo, &todo_path)?
    } else if let Some(oid) = read_cherry_pick_head(git_dir)? {
        vec![TodoEntry {
            verb: TodoVerb::Pick,
            oid,
            subject: commit_subject(repo, oid)?,
        }]
    } else if let Some(oid) = read_revert_head(git_dir)? {
        vec![TodoEntry {
            verb: TodoVerb::Revert,
            oid,
            subject: commit_subject(repo, oid)?,
        }]
    } else {
        return Err(Error::NoReplayInProgress);
    };
    let abort_safety = read_sequencer_abort_safety(git_dir)
        .or_else(|| read_orig_head(git_dir).ok().flatten())
        .or_else(|| resolve_head(git_dir).ok().and_then(|h| h.oid().copied()))
        .ok_or(Error::NoReplayInProgress)?;
    let (direction, opts) = load_persisted_options(git_dir, &todo)?;
    Ok(ActiveReplay {
        direction,
        opts,
        todo,
        abort_safety,
    })
}

impl ActiveReplay {
    fn current_source(&self) -> Result<Option<ObjectId>> {
        Ok(self.todo.first().map(|t| t.oid))
    }
}

fn read_sequencer_head(git_dir: &Path) -> Option<ObjectId> {
    read_oid_file(&git_dir.join("sequencer").join("head"))
}

fn read_sequencer_abort_safety(git_dir: &Path) -> Option<ObjectId> {
    read_oid_file(&git_dir.join("sequencer").join("abort-safety"))
}

fn read_oid_file(path: &Path) -> Option<ObjectId> {
    fs::read_to_string(path)
        .ok()
        .and_then(|s| ObjectId::from_hex(s.trim()).ok())
}

fn parse_sequencer_todo(repo: &Repository, path: &Path) -> Result<Vec<TodoEntry>> {
    let content = fs::read_to_string(path).map_err(Error::Io)?;
    let mut out = Vec::new();
    for line in content.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        let mut parts = t.split_whitespace();
        let cmd = parts.next().ok_or_else(|| Error::SequencerUnknownCommand {
            command: String::new(),
        })?;
        let (verb, direction_hint) = match cmd {
            "pick" | "p" => (TodoVerb::Pick, ReplayDirection::Pick),
            "revert" | "r" => (TodoVerb::Revert, ReplayDirection::Revert),
            other => {
                return Err(Error::SequencerUnknownCommand {
                    command: other.to_owned(),
                });
            }
        };
        let oid_token = parts.next().ok_or_else(|| Error::SequencerUnknownCommand {
            command: cmd.to_owned(),
        })?;
        let oid =
            repo.rev_walk()
                .resolve(oid_token)
                .map_err(|_| Error::SequencerUnknownCommand {
                    command: cmd.to_owned(),
                })?;
        let subject = parts.collect::<Vec<_>>().join(" ");
        let subject = if subject.is_empty() {
            commit_subject(repo, oid)?
        } else {
            subject
        };
        let _ = direction_hint;
        out.push(TodoEntry { verb, oid, subject });
    }
    if out.is_empty() {
        return Err(Error::NoReplayInProgress);
    }
    Ok(out)
}

fn load_persisted_options(
    git_dir: &Path,
    todo: &[TodoEntry],
) -> Result<(ReplayDirection, ReplayOptions)> {
    let path = git_dir.join("sequencer").join("grit-opts");
    let mut opts = ReplayOptions::default();
    let mut direction = todo
        .first()
        .map(|e| match e.verb {
            TodoVerb::Pick => ReplayDirection::Pick,
            TodoVerb::Revert => ReplayDirection::Revert,
        })
        .unwrap_or(ReplayDirection::Pick);
    if read_cherry_pick_head(git_dir)?.is_some() {
        direction = ReplayDirection::Pick;
    } else if read_revert_head(git_dir)?.is_some() {
        direction = ReplayDirection::Revert;
    }
    apply_git_sequencer_opts(git_dir, &mut opts);
    if !path.is_file() {
        return Ok((direction, opts));
    }
    let content = fs::read_to_string(&path).map_err(Error::Io)?;
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "direction" => {
                direction = if value == "revert" {
                    ReplayDirection::Revert
                } else {
                    ReplayDirection::Pick
                };
            }
            "record-origin" => opts.record_origin = value == "true",
            "no-commit" => opts.no_commit = value == "true",
            "empty" => {
                opts.empty = match value {
                    "drop" => ReplayEmptyPolicy::Drop,
                    "keep" => ReplayEmptyPolicy::Keep,
                    _ => ReplayEmptyPolicy::Stop,
                };
            }
            "favor" => {
                opts.favor = match value {
                    "ours" => MergeFavor::Ours,
                    "theirs" => MergeFavor::Theirs,
                    "union" => MergeFavor::Union,
                    _ => MergeFavor::None,
                };
            }
            "conflict-style" => {
                opts.conflict_style = match value {
                    "diff3" => ConflictStyle::Diff3,
                    "zealous-diff3" => ConflictStyle::ZealousDiff3,
                    _ => ConflictStyle::Merge,
                };
            }
            "mainline" => {
                if value.is_empty() {
                    opts.mainline = None;
                } else if let Ok(n) = value.parse::<u32>() {
                    opts.mainline = NonZeroU32::new(n);
                }
            }
            "message-override" if !value.is_empty() => {
                opts.message_override = Some(value.to_owned());
            }
            _ => {}
        }
    }
    Ok((direction, opts))
}

fn clear_step_heads(git_dir: &Path, direction: ReplayDirection) -> Result<()> {
    let name = match direction {
        ReplayDirection::Pick => "CHERRY_PICK_HEAD",
        ReplayDirection::Revert => "REVERT_HEAD",
    };
    let path = git_dir.join(name);
    if path.exists() {
        fs::remove_file(path).map_err(Error::Io)?;
    }
    Ok(())
}

fn clear_replay_state(git_dir: &Path) -> Result<()> {
    for name in ["CHERRY_PICK_HEAD", "REVERT_HEAD", "ORIG_HEAD", "MERGE_MSG"] {
        let path = git_dir.join(name);
        if path.exists() {
            fs::remove_file(path).map_err(Error::Io)?;
        }
    }
    let seq = git_dir.join("sequencer");
    if seq.is_dir() {
        fs::remove_dir_all(seq).map_err(Error::Io)?;
    }
    Ok(())
}

fn reset_index_worktree_to_head(repo: &Repository, head_oid: &ObjectId) -> Result<()> {
    let head_tree = commit_tree(repo, head_oid)?;
    let mut index = index_from_tree_with_worktree_stats(repo, &head_tree)?;
    repo.write_index(&mut index)?;
    checkout_between_trees(repo, None, &head_tree)?;
    Ok(())
}

fn hard_reset_branch(repo: &Repository, refname: &str, target: ObjectId) -> Result<()> {
    let short = refname.strip_prefix("refs/heads/").unwrap_or(refname);
    let _ = refname;
    reset_to_commit(
        repo,
        &ResetOptions {
            target,
            mode: ResetMode::Hard,
            identity: "Grit <grit@local> 0 +0000".to_owned(),
            message: "abort replay".to_owned(),
        },
    )?;
    let _ = short;
    Ok(())
}

fn index_stage0_tree(repo: &Repository) -> Result<ObjectId> {
    let mut index = repo.load_index()?;
    write_tree_update_index(repo.odb(), &mut index, "", WriteTreeFlags::silent())
}

fn apply_git_sequencer_opts(git_dir: &Path, opts: &mut ReplayOptions) {
    let path = git_dir.join("sequencer").join("opts");
    let Ok(content) = fs::read_to_string(&path) else {
        return;
    };
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        if key == "record-origin" && value == "true" {
            opts.record_origin = true;
        }
    }
}

fn commit_tree(repo: &Repository, commit_oid: &ObjectId) -> Result<ObjectId> {
    let obj = repo.odb().read(commit_oid)?;
    if obj.kind != ObjectKind::Commit {
        return Err(Error::CorruptObject(format!(
            "expected commit, got {}",
            obj.kind.as_str()
        )));
    }
    Ok(parse_commit(&obj.data)?.tree)
}
