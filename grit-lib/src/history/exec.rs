//! Run `rebase --exec` hooks after planning and before ref updates.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use crate::command_runner::{
    run_sh_dash_c, CommandOutput, CommandRunner, CommandStdin, CommandStdio,
};
use crate::error::Error;
use crate::merge_file::{ConflictStyle, MergeFavor};
use crate::merge_trees::{
    materialized_merge_result_tree, merge_trees_three_way, DirectoryRenameMode,
    WhitespaceMergeOptions,
};
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use crate::porcelain::merge::apply_conflict_merge_worktree;
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local, verify_tree_transition_allow_local,
    TreeTransitionVerifyError,
};
use crate::porcelain::worktree_guard::{
    ensure_worktree_clean_for_rebase_exec, ensure_worktree_clean_from_snapshot,
    load_worktree_index, WorktreeSnapshot,
};
use crate::refs::{update_direct_ref_on_store, write_symbolic_ref_on_store};
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

use super::apply::commit_tree_for_apply;
use super::error::HistoryError;
use super::plan::{commit_parents, replay_commit_onto_mapping, write_resolved_replay};
use super::subject::replay_conflict_labels;
use super::types::{RewriteOptions, RewritePlan};

/// Shell commands to run at each replayed commit during `rebase --exec`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecSpec {
    /// Commands executed in order at every rewritten commit (`sh -c`).
    pub commands: Vec<String>,
}

/// One `--exec` invocation at a specific commit.
#[derive(Debug, Clone, PartialEq, Eq)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct ExecRunRecord {
    /// Commit checked out when the command ran.
    pub commit: ObjectId,
    /// Shell command string.
    pub command: String,
    /// Subprocess exit code (`1` when the OS provides no code).
    pub exit_code: i32,
    /// Captured stdout bytes (serialized as lossy UTF-8 text).
    #[cfg_attr(
        feature = "serde-models",
        serde(serialize_with = "serialize_lossy_utf8")
    )]
    pub stdout: Vec<u8>,
    /// Captured stderr bytes (serialized as lossy UTF-8 text).
    #[cfg_attr(
        feature = "serde-models",
        serde(serialize_with = "serialize_lossy_utf8")
    )]
    pub stderr: Vec<u8>,
}

#[cfg(feature = "serde-models")]
fn serialize_lossy_utf8<S: serde::Serializer>(
    bytes: &[u8],
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&String::from_utf8_lossy(bytes))
}

/// Successful exec invocations captured during one verification or recovery call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde-models", derive(serde::Serialize))]
pub struct ExecReport {
    /// One row per command invocation, in execution order.
    pub runs: Vec<ExecRunRecord>,
}

#[derive(Debug, Clone)]
enum SavedHead {
    Symbolic(String),
    Detached(ObjectId),
}

#[derive(Debug, Clone)]
struct ExecPrestate {
    head: SavedHead,
    head_commit: ObjectId,
    index_bytes: Vec<u8>,
    head_tree: ObjectId,
}

struct ExecDetachedSession<'a> {
    repo: &'a Repository,
    prestate: &'a ExecPrestate,
    current_tree: ObjectId,
    active: bool,
}

impl ExecDetachedSession<'_> {
    fn restore(&mut self) -> Result<(), HistoryError> {
        if !self.active {
            return Ok(());
        }
        self.active = false;
        restore_exec_prestate(self.repo, self.prestate, self.current_tree)
    }
}

fn fail_exec_session(session: &mut ExecDetachedSession<'_>, err: HistoryError) -> HistoryError {
    match session.restore() {
        Ok(()) => {
            let _ = super::rebase_session::clear_rebase_state(session.repo.git_dir());
            err
        }
        Err(restore_err) => restore_err,
    }
}

fn exec_command_signaled(output: &CommandOutput) -> bool {
    !output.status.success && output.status.code.is_none()
}

/// How exec commands and the replays after them changed a plan.
///
/// Keys of `remap` are the replacements the plan originally held, so later rows (whose
/// parents name those commits) can be replayed onto whatever now stands in for them.
#[derive(Debug, Clone)]
struct ExecRewrites {
    /// Replacement each plan row held before exec ran.
    planned: Vec<Option<ObjectId>>,
    /// Planned replacement to the commit that now ends that row's line of history: an
    /// amended commit, extra commits a command added, or a replay onto either.
    remap: HashMap<ObjectId, ObjectId>,
}

/// Where a successful command left the detached `HEAD`, relative to the commit it ran at.
enum HeadMove {
    /// New commits on top of the commit.
    Extends,
    /// This commit replaces the one the command ran at (same first parent), possibly with
    /// new commits on top of it.
    Replaces(ObjectId),
}

impl ExecRewrites {
    fn new(plan: &RewritePlan) -> Self {
        Self {
            planned: plan.commits.iter().map(|row| row.new).collect(),
            remap: HashMap::new(),
        }
    }

    /// Replay row `row` when one of its parents was rewritten; a row whose replacement
    /// already differs from the planned one was settled earlier.
    fn replay_row(
        &mut self,
        repo: &Repository,
        plan: &mut RewritePlan,
        row: usize,
        opts: &RewriteOptions,
    ) -> Result<(), HistoryError> {
        let Some(planned) = self.planned[row] else {
            return Ok(());
        };
        if plan.commits[row].new != Some(planned) {
            return Ok(());
        }
        let parents = commit_parents(repo, planned)?;
        if !parents.iter().any(|p| self.remap.contains_key(p)) {
            return Ok(());
        }
        let replayed = replay_commit_onto_mapping(repo, &self.remap, planned, opts)?;
        let new_first = parents
            .first()
            .map(|p| self.remap.get(p).copied().unwrap_or(*p));
        let entry = &mut plan.commits[row];
        if new_first == Some(replayed) {
            entry.new = None;
            entry.change_id = None;
        } else {
            set_row_replacement(repo, entry, replayed)?;
        }
        self.remap.insert(planned, replayed);
        Ok(())
    }

    /// Whether row `row` replays a single-parent commit (a conflict there can stop).
    fn replays_single_parent(&self, repo: &Repository, row: usize) -> bool {
        self.planned[row]
            .and_then(|planned| commit_parents(repo, planned).ok())
            .is_some_and(|parents| parents.len() == 1)
    }

    /// Record `tip` as the end of row `row`'s line of history.
    fn set_tip(&mut self, row: usize, tip: ObjectId) {
        let Some(planned) = self.planned[row] else {
            return;
        };
        if tip == planned {
            self.remap.remove(&planned);
        } else {
            self.remap.insert(planned, tip);
        }
    }

    fn tip(&self, plan: &RewritePlan, row: usize) -> Option<ObjectId> {
        let current = plan.commits[row].new?;
        Some(
            self.planned[row]
                .and_then(|planned| self.remap.get(&planned).copied())
                .unwrap_or(current),
        )
    }

    /// Point ref updates and the `HEAD` effect at the rewritten commits.
    fn finish(&self, repo: &Repository, plan: &mut RewritePlan) -> Result<(), HistoryError> {
        if self.remap.is_empty() {
            return Ok(());
        }
        for update in &mut plan.ref_updates {
            if let Some(new) = self.remap.get(&update.new) {
                update.new = *new;
            }
        }
        let head_name = plan.head.head_ref.as_deref().unwrap_or("HEAD");
        if !plan
            .ref_updates
            .iter()
            .any(|update| update.name == head_name)
        {
            if let Some((old, new)) = plan
                .head
                .old_commit
                .and_then(|old| self.remap.get(&old).map(|new| (old, *new)))
            {
                plan.ref_updates.push(super::PlannedRefUpdate {
                    name: head_name.to_owned(),
                    old,
                    new,
                });
                plan.ref_updates.sort_by(|a, b| a.name.cmp(&b.name));
            }
        }
        if let Some(new) = plan
            .ref_updates
            .iter()
            .find(|update| update.name == head_name)
            .map(|update| update.new)
        {
            plan.head.new_tree = Some(commit_tree_for_apply(repo, new)?);
            plan.head.commit_changes = plan.head.old_commit != Some(new);
        }
        plan.ref_updates.retain(|update| update.new != update.old);
        Ok(())
    }
}

fn set_row_replacement(
    repo: &Repository,
    row: &mut super::RewrittenCommit,
    commit: ObjectId,
) -> Result<(), HistoryError> {
    let data = repo.odb().read_commit(&commit)?;
    row.new = Some(commit);
    row.change_id = crate::change_id::logical_change_id_for_commit(repo, commit)?;
    row.subject = Some(super::subject::commit_message_subject_line(&data.message));
    row.reused = false;
    Ok(())
}

/// Classify how a command moved `HEAD` from `tip` to `head` (first-parent walk from `head`).
/// `None` when `head` neither replaces nor extends `tip`.
fn classify_head_move(
    repo: &Repository,
    tip: ObjectId,
    head: ObjectId,
) -> Result<Option<HeadMove>, HistoryError> {
    let base = commit_parents(repo, tip)?.first().copied();
    let mut current = head;
    loop {
        if current == tip {
            return Ok(Some(HeadMove::Extends));
        }
        if Some(current) == base {
            return Ok(None);
        }
        let first = commit_parents(repo, current)?.first().copied();
        if first == base {
            return Ok(Some(HeadMove::Replaces(current)));
        }
        match first {
            Some(parent) => current = parent,
            None => return Ok(None),
        }
    }
}

/// Where replay resumes: plan row index and the next command at that row.
#[derive(Debug, Clone, Copy)]
struct ExecCursor {
    row: usize,
    command: usize,
    /// `HEAD` the commands at `row` run at when that row has no replacement (its
    /// conflicted replay was skipped, so pending commands run at the parent, as in Git).
    head: Option<ObjectId>,
}

/// Checkout each rewritten commit in plan order and run `spec.commands`, then restore
/// `HEAD`, the index, and the worktree on clean completion. Branch refs are not modified;
/// apply the updated `plan` afterwards.
///
/// A command may rewrite the commit it runs at, the way `git rebase --exec` allows: after
/// it exits zero, a detached `HEAD` that amends the replay commit (same first parent) or
/// adds commits on top of it is adopted. `plan` is updated so the amended commit replaces
/// the planned one, later commits are replayed onto the new `HEAD` with `rewrite`, and ref
/// updates and [`RewritePlan::head`] point at the result. Later commands see the
/// rewritten trees. On error, `plan` may be partially updated and should be discarded.
///
/// # Errors
///
/// - [`HistoryError::BareRepository`] in a bare repository.
/// - [`HistoryError::UncommittedChanges`] when the worktree or index is dirty.
/// - [`HistoryError::LocalChangesWouldBeOverwritten`] when a checkout would clobber local paths.
/// - [`HistoryError::ExecFailed`] when a command exits non-zero (state is rolled back).
/// - [`HistoryError::ExecHeadMoved`] when a command leaves `HEAD` anywhere else (state is
///   rolled back).
/// - [`HistoryError::ExecDirty`] when a successful command leaves the index or worktree
///   different from the resulting `HEAD`. The edits remain at the detached `HEAD`, branch
///   refs stay unchanged, and exec recovery state is saved in `rebase-merge/grit-exec`.
///   Save or remove the edits before [`super::continue_rebase`] or [`super::skip_rebase`];
///   [`super::abort_rebase`] explicitly discards them and restores the original state.
/// - [`HistoryError::ExecConflict`] when replaying a later commit onto a rewritten one
///   conflicts. The conflict is checked out and exec recovery state is saved; resolve and
///   [`super::continue_rebase`], or [`super::skip_rebase`] / [`super::abort_rebase`].
/// - [`HistoryError::Conflict`] when such a replay of a merge commit conflicts, and
///   [`HistoryError::BecameEmpty`] when a replay becomes empty under
///   [`super::EmptyPolicy::Abort`] (state is rolled back).
/// - I/O, checkout, or subprocess failures.
pub fn verify_rebase_with_exec(
    repo: &Repository,
    plan: &mut RewritePlan,
    spec: &ExecSpec,
    rewrite: &RewriteOptions,
    runner: &dyn CommandRunner,
) -> Result<ExecReport, HistoryError> {
    if spec.commands.is_empty() {
        return Ok(ExecReport::default());
    }
    if repo.is_bare() {
        return Err(HistoryError::BareRepository);
    }
    if repo.git_dir().join("rebase-merge").exists() || repo.git_dir().join("rebase-apply").exists()
    {
        return Err(exec_io("a rebase is already in progress"));
    }
    ensure_worktree_clean_for_rebase_exec(repo).map_err(map_clean_error)?;

    let prestate = capture_exec_prestate(repo)?;
    if plan.commits.iter().all(|row| row.new.is_none()) {
        return Ok(ExecReport::default());
    }

    let mut session = ExecDetachedSession {
        repo,
        prestate: &prestate,
        current_tree: prestate.head_tree,
        active: false,
    };
    let mut rewrites = ExecRewrites::new(plan);
    execute_plan(
        &mut session,
        plan,
        &mut rewrites,
        spec,
        rewrite,
        runner,
        ExecCursor {
            row: 0,
            command: 0,
            head: None,
        },
    )
}

/// Read the detached `HEAD` a successful command left behind.
fn exec_head(repo: &Repository, tip: ObjectId) -> Result<ObjectId, HistoryError> {
    match resolve_head_for_repository(repo).map_err(exec_io)? {
        HeadState::Detached { oid } => Ok(oid),
        other => Err(HistoryError::ExecHeadMoved {
            commit: tip,
            head: other.oid().copied(),
        }),
    }
}

fn execute_plan(
    session: &mut ExecDetachedSession<'_>,
    plan: &mut RewritePlan,
    rewrites: &mut ExecRewrites,
    spec: &ExecSpec,
    rewrite: &RewriteOptions,
    runner: &dyn CommandRunner,
    start: ExecCursor,
) -> Result<ExecReport, HistoryError> {
    let repo = session.repo;
    let mut runs = Vec::new();
    let worktree = repo.work_tree().ok_or(HistoryError::BareRepository)?;
    for row in start.row..plan.commits.len() {
        session.active = true;
        if let Err(err) = rewrites.replay_row(repo, plan, row, rewrite) {
            return Err(match err {
                HistoryError::Conflict { conflicts, .. }
                    if rewrites.replays_single_parent(repo, row) =>
                {
                    stop_on_replay_conflict(
                        session, plan, rewrites, spec, rewrite, row, conflicts, runs,
                    )
                }
                err => fail_exec_session(session, err),
            });
        }
        let fallback = start.head.filter(|_| row == start.row);
        let Some(mut tip) = rewrites.tip(plan, row).or(fallback) else {
            continue;
        };
        let mut tip_tree = commit_tree_for_apply(repo, tip)?;
        let first_command = if row == start.row { start.command } else { 0 };
        store_exec_stop(
            session,
            plan,
            rewrites,
            spec,
            rewrite,
            ExecStop {
                cursor: ExecCursor {
                    row,
                    command: first_command,
                    head: None,
                },
                head: tip,
                pending_pick: false,
            },
        )?;
        update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &tip).map_err(|e| {
            HistoryError::WorktreeApply {
                message: e.to_string(),
            }
        })?;
        if let Err(err) =
            checkout_exec_tree_transition(repo, Some(&session.current_tree), &tip_tree)
        {
            return Err(fail_exec_session(session, err));
        }
        session.current_tree = tip_tree;

        for (command_index, command) in spec.commands.iter().enumerate().skip(first_command) {
            store_exec_stop(
                session,
                plan,
                rewrites,
                spec,
                rewrite,
                ExecStop {
                    cursor: ExecCursor {
                        row,
                        command: command_index,
                        head: None,
                    },
                    head: tip,
                    pending_pick: false,
                },
            )?;
            let output = match run_exec_command(runner, command, worktree) {
                Ok(output) => output,
                Err(err) => return Err(fail_exec_session(session, err)),
            };
            let signaled = exec_command_signaled(&output);
            let exit_code = output.status.code.unwrap_or(1);
            let stdout = output.stdout;
            let stderr = output.stderr;
            runs.push(ExecRunRecord {
                commit: tip,
                command: command.clone(),
                exit_code,
                stdout: stdout.clone(),
                stderr: stderr.clone(),
            });
            if exit_code != 0 {
                let (completed_runs, stdout, stderr) = match runs.split_last() {
                    Some((failed, prior)) => {
                        (prior.to_vec(), failed.stdout.clone(), failed.stderr.clone())
                    }
                    None => (Vec::new(), stdout, stderr),
                };
                let exec_err = HistoryError::ExecFailed {
                    commit: tip,
                    exit_code,
                    stdout,
                    stderr,
                    completed_runs,
                };
                if signaled {
                    session.active = false;
                    return Err(exec_err);
                }
                return Err(fail_exec_session(session, exec_err));
            }
            match adopt_exec_head(repo, plan, rewrites, row, tip) {
                Ok(Some(head)) => {
                    tip = head;
                    tip_tree = commit_tree_for_apply(repo, tip)?;
                    session.current_tree = tip_tree;
                }
                Ok(None) => {}
                Err(err) => {
                    if let Some(tree) = resolve_head_for_repository(repo)
                        .ok()
                        .and_then(|head| head.oid().copied())
                        .and_then(|oid| commit_tree_for_apply(repo, oid).ok())
                    {
                        session.current_tree = tree;
                    }
                    return Err(fail_exec_session(session, err));
                }
            }
            // Reload the subprocess-written index and compare against the resulting HEAD,
            // including racily clean worktree entries, before any further mutation.
            let clean = load_worktree_index(repo).and_then(|index| {
                let mut snapshot = WorktreeSnapshot {
                    index,
                    head_tree: Some(tip_tree),
                };
                ensure_worktree_clean_from_snapshot(
                    repo,
                    &mut snapshot,
                    crate::error::WorktreeOperation::RebaseExec,
                    false,
                )
            });
            if let Err(err) = clean {
                session.active = false;
                store_exec_stop(
                    session,
                    plan,
                    rewrites,
                    spec,
                    rewrite,
                    ExecStop {
                        cursor: ExecCursor {
                            row,
                            command: command_index + 1,
                            head: None,
                        },
                        head: tip,
                        pending_pick: false,
                    },
                )?;
                return Err(match err {
                    Error::DirtyWorktree { paths, .. } => HistoryError::ExecDirty {
                        commit: tip,
                        command: command.clone(),
                        paths,
                        runs,
                    },
                    other => exec_io(other),
                });
            }
            store_exec_stop(
                session,
                plan,
                rewrites,
                spec,
                rewrite,
                ExecStop {
                    cursor: ExecCursor {
                        row,
                        command: command_index + 1,
                        head: None,
                    },
                    head: tip,
                    pending_pick: false,
                },
            )?;
        }
    }

    rewrites.finish(repo, plan)?;
    session.restore()?;
    super::rebase_session::clear_rebase_state(repo.git_dir()).map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })?;
    Ok(ExecReport { runs })
}

/// Adopt the detached `HEAD` a successful command left at row `row` (checked out at `tip`).
/// Returns the new tip when `HEAD` moved.
fn adopt_exec_head(
    repo: &Repository,
    plan: &mut RewritePlan,
    rewrites: &mut ExecRewrites,
    row: usize,
    tip: ObjectId,
) -> Result<Option<ObjectId>, HistoryError> {
    let head = exec_head(repo, tip)?;
    if head == tip {
        return Ok(None);
    }
    match classify_head_move(repo, tip, head)? {
        Some(HeadMove::Extends) => {}
        Some(HeadMove::Replaces(replacement)) => {
            // Amending an extra commit a command added keeps the row's own replacement.
            if plan.commits[row].new == Some(tip) {
                set_row_replacement(repo, &mut plan.commits[row], replacement)?;
            }
        }
        None => {
            return Err(HistoryError::ExecHeadMoved {
                commit: tip,
                head: Some(head),
            })
        }
    }
    rewrites.set_tip(row, head);
    Ok(Some(head))
}

/// Check out the conflicted replay of row `row` on top of its rewritten parent and save
/// exec recovery state with that replay pending. If the state cannot be saved, the
/// session is rolled back so no detached conflict is left without a recovery record.
#[allow(clippy::too_many_arguments)]
fn stop_on_replay_conflict(
    session: &mut ExecDetachedSession<'_>,
    plan: &RewritePlan,
    rewrites: &ExecRewrites,
    spec: &ExecSpec,
    rewrite: &RewriteOptions,
    row: usize,
    conflicts: Vec<crate::operation::Conflict>,
    runs: Vec<ExecRunRecord>,
) -> HistoryError {
    let repo = session.repo;
    let mut checkout = || -> Result<ObjectId, HistoryError> {
        let planned = rewrites.planned[row].ok_or_else(|| exec_io("invalid exec replay row"))?;
        let data = repo.odb().read_commit(&planned)?;
        let old_parent = *data
            .parents
            .first()
            .ok_or(HistoryError::RootCommit { commit: planned })?;
        let parent = rewrites
            .remap
            .get(&old_parent)
            .copied()
            .unwrap_or(old_parent);
        let parent_tree = commit_tree_for_apply(repo, parent)?;
        update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &parent)
            .map_err(exec_io)?;
        checkout_exec_tree_transition(repo, Some(&session.current_tree), &parent_tree)?;
        session.current_tree = parent_tree;
        let merge = merge_trees_three_way(
            repo,
            commit_tree_for_apply(repo, old_parent)?,
            parent_tree,
            data.tree,
            MergeFavor::default(),
            WhitespaceMergeOptions::default(),
            None,
            replay_conflict_labels(plan.commits[row].old, &data).presentation(ConflictStyle::Merge),
            DirectoryRenameMode::from_repo(repo)?,
        )?;
        let materialized = materialized_merge_result_tree(repo.odb(), &merge)?;
        apply_conflict_merge_worktree(
            repo,
            &parent_tree,
            &merge,
            &materialized,
            &mut crate::progress::NullProgress,
            &crate::progress::NeverCancel,
        )?;
        Ok(parent)
    };
    let parent = match checkout() {
        Ok(parent) => parent,
        Err(err) => return fail_exec_session(session, err),
    };
    let stop = ExecStop {
        cursor: ExecCursor {
            row,
            command: 0,
            head: None,
        },
        head: parent,
        pending_pick: true,
    };
    if let Err(err) = store_exec_stop(session, plan, rewrites, spec, rewrite, stop) {
        return fail_exec_session(session, err);
    }
    session.active = false;
    HistoryError::ExecConflict {
        commit: plan.commits[row].old,
        conflicts,
        runs,
    }
}

fn checkout_exec_tree_transition(
    repo: &Repository,
    from_tree: Option<&ObjectId>,
    to_tree: &ObjectId,
) -> Result<(), HistoryError> {
    match verify_tree_transition_allow_local(repo, from_tree, to_tree) {
        Ok(()) => {}
        Err(TreeTransitionVerifyError::WouldOverwrite(paths)) => {
            return Err(HistoryError::LocalChangesWouldBeOverwritten { paths });
        }
        Err(TreeTransitionVerifyError::Failed(err)) => {
            return Err(HistoryError::WorktreeApply {
                message: err.to_string(),
            });
        }
    }
    checkout_tree_transition_allow_local(repo, from_tree, to_tree).map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })
}

fn run_exec_command(
    runner: &dyn CommandRunner,
    command: &str,
    worktree: &Path,
) -> Result<CommandOutput, HistoryError> {
    run_sh_dash_c(
        runner,
        command,
        Some(worktree),
        CommandStdin::Null,
        CommandStdio::Pipe,
    )
    .map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })
}

fn map_clean_error(err: crate::error::Error) -> HistoryError {
    match err {
        crate::error::Error::DirtyWorktree { .. } => HistoryError::UncommittedChanges,
        other => HistoryError::WorktreeApply {
            message: other.to_string(),
        },
    }
}

fn capture_exec_prestate(repo: &Repository) -> Result<ExecPrestate, HistoryError> {
    let head_state =
        resolve_head_for_repository(repo).map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
    // The clean check ran against this same HEAD, so the saved HEAD and the tree restored
    // afterwards must come from this one read; mixing sources restores a HEAD that does
    // not match the restored index and worktree.
    let (head, head_commit) = match head_state {
        HeadState::Branch {
            refname,
            oid: Some(oid),
            ..
        } => (SavedHead::Symbolic(refname), oid),
        HeadState::Detached { oid } => (SavedHead::Detached(oid), oid),
        HeadState::Branch { oid: None, .. } => return Err(Error::UnbornHead.into()),
        HeadState::Invalid => {
            return Err(HistoryError::WorktreeApply {
                message: "HEAD is in an unknown state".into(),
            });
        }
    };
    let head_tree = commit_tree_for_apply(repo, head_commit)?;
    let index_bytes = fs::read(repo.index_path()).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    Ok(ExecPrestate {
        head,
        head_commit,
        index_bytes,
        head_tree,
    })
}

fn restore_exec_prestate(
    repo: &Repository,
    pre: &ExecPrestate,
    current_tree: ObjectId,
) -> Result<(), HistoryError> {
    match &pre.head {
        SavedHead::Symbolic(refname) => {
            if repo.resolve_ref_oid(refname)? != pre.head_commit {
                return Err(HistoryError::RefCasMismatch {
                    name: refname.clone(),
                });
            }
            write_symbolic_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", refname).map_err(
                |e| HistoryError::WorktreeApply {
                    message: e.to_string(),
                },
            )?;
        }
        SavedHead::Detached(oid) => {
            update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", oid).map_err(|e| {
                HistoryError::WorktreeApply {
                    message: e.to_string(),
                }
            })?;
        }
    }
    checkout_hard_to_tree_from_head_tree(
        repo,
        &current_tree,
        &pre.head_tree,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )
    .map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    fs::write(repo.index_path(), &pre.index_bytes).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    Ok(())
}

/// Position and expected `HEAD` of an exec stop.
struct ExecStop {
    cursor: ExecCursor,
    /// Detached `HEAD` recovery expects.
    head: ObjectId,
    /// The stop is a conflicted replay of `cursor.row` on top of `head`.
    pending_pick: bool,
}

// The exec stop is deliberately distinct from a conflicted pick: the current commit
// is already replayed, and continuing must never commit its index as a resolution.
// The one exception is a pending pick, where the replay itself conflicted.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredExec {
    head_ref: Option<String>,
    original_head: String,
    original_tree: String,
    index: Vec<u8>,
    commits: Vec<(String, Option<String>, bool)>,
    planned: Vec<Option<String>>,
    remap: Vec<(String, String)>,
    refs: Vec<(String, String, String)>,
    checkout_onto: Option<String>,
    new_tree: Option<String>,
    commit_changes: bool,
    commands: Vec<String>,
    rewrite: RewriteOptions,
    row: usize,
    next_command: usize,
    head: String,
    pending_pick: bool,
}

fn exec_io(err: impl std::fmt::Display) -> HistoryError {
    HistoryError::WorktreeApply {
        message: err.to_string(),
    }
}

/// Write the exec recovery record. `grit-exec` is committed last, so on failure no
/// recovery record exists and the partial `orig-head` / `head-name` are removed.
fn store_exec_stop(
    session: &ExecDetachedSession<'_>,
    plan: &RewritePlan,
    rewrites: &ExecRewrites,
    spec: &ExecSpec,
    rewrite: &RewriteOptions,
    stop: ExecStop,
) -> Result<(), HistoryError> {
    use std::io::Write;
    let pre = session.prestate;
    let state = StoredExec {
        head_ref: match &pre.head {
            SavedHead::Symbolic(name) => Some(name.clone()),
            SavedHead::Detached(_) => None,
        },
        original_head: pre.head_commit.to_hex(),
        original_tree: pre.head_tree.to_hex(),
        index: pre.index_bytes.clone(),
        commits: plan
            .commits
            .iter()
            .map(|r| (r.old.to_hex(), r.new.map(|o| o.to_hex()), r.reused))
            .collect(),
        planned: rewrites
            .planned
            .iter()
            .map(|o| o.map(|o| o.to_hex()))
            .collect(),
        remap: rewrites
            .remap
            .iter()
            .map(|(k, v)| (k.to_hex(), v.to_hex()))
            .collect(),
        refs: plan
            .ref_updates
            .iter()
            .map(|r| (r.name.clone(), r.old.to_hex(), r.new.to_hex()))
            .collect(),
        checkout_onto: plan.head.checkout_onto.map(|o| o.to_hex()),
        new_tree: plan.head.new_tree.map(|o| o.to_hex()),
        commit_changes: plan.head.commit_changes,
        commands: spec.commands.clone(),
        rewrite: rewrite.clone(),
        row: stop.cursor.row,
        next_command: stop.cursor.command,
        head: stop.head.to_hex(),
        pending_pick: stop.pending_pick,
    };
    let dir = session.repo.git_dir().join("rebase-merge");
    let write = || -> Result<(), HistoryError> {
        fs::create_dir_all(&dir).map_err(exec_io)?;
        let mut lock = crate::lockfile::LockFile::acquire(
            &dir.join("grit-exec.lock"),
            crate::lockfile::LockWait::Immediately,
        )
        .map_err(exec_io)?;
        lock.write_all(&serde_json::to_vec(&state).map_err(exec_io)?)
            .map_err(exec_io)?;
        fs::write(dir.join("orig-head"), format!("{}\n", state.original_head)).map_err(exec_io)?;
        fs::write(
            dir.join("head-name"),
            format!("{}\n", state.head_ref.as_deref().unwrap_or("detached HEAD")),
        )
        .map_err(exec_io)?;
        lock.commit_to(&dir.join("grit-exec")).map_err(exec_io)
    };
    write().inspect_err(|_| {
        for name in ["orig-head", "head-name"] {
            let _ = fs::remove_file(dir.join(name));
        }
        let _ = fs::remove_dir(&dir);
    })
}

fn load_exec_stop(repo: &Repository) -> Result<StoredExec, HistoryError> {
    serde_json::from_slice(
        &fs::read(repo.git_dir().join("rebase-merge/grit-exec")).map_err(exec_io)?,
    )
    .map_err(exec_io)
}

fn stored_prestate(state: &StoredExec) -> Result<ExecPrestate, HistoryError> {
    let head_commit = ObjectId::from_hex(&state.original_head)?;
    Ok(ExecPrestate {
        head: state
            .head_ref
            .as_ref()
            .map_or(SavedHead::Detached(head_commit), |name| {
                SavedHead::Symbolic(name.clone())
            }),
        head_commit,
        head_tree: ObjectId::from_hex(&state.original_tree)?,
        index_bytes: state.index.clone(),
    })
}

fn stored_plan(
    repo: &Repository,
    state: &StoredExec,
    prestate: &ExecPrestate,
) -> Result<RewritePlan, HistoryError> {
    use super::{HeadEffect, PlannedRefUpdate, RewrittenCommit};
    let parse_opt = |hex: &Option<String>| hex.as_deref().map(ObjectId::from_hex).transpose();
    Ok(RewritePlan {
        commits: state
            .commits
            .iter()
            .map(|(old, new, reused)| {
                let old = ObjectId::from_hex(old)?;
                let new = new.as_deref().map(ObjectId::from_hex).transpose()?;
                let data = repo.odb().read_commit(&new.unwrap_or(old))?;
                Ok(RewrittenCommit {
                    old,
                    new,
                    change_id: new
                        .map(|oid| crate::change_id::logical_change_id_for_commit(repo, oid))
                        .transpose()?
                        .flatten(),
                    subject: Some(super::subject::commit_message_subject_line(&data.message)),
                    reused: *reused,
                })
            })
            .collect::<Result<_, HistoryError>>()?,
        ref_updates: state
            .refs
            .iter()
            .map(|(name, old, new)| {
                Ok(PlannedRefUpdate {
                    name: name.clone(),
                    old: ObjectId::from_hex(old)?,
                    new: ObjectId::from_hex(new)?,
                })
            })
            .collect::<Result<_, HistoryError>>()?,
        head: HeadEffect {
            commit_changes: state.commit_changes,
            old_commit: Some(prestate.head_commit),
            head_ref: state.head_ref.clone(),
            old_tree: Some(prestate.head_tree),
            new_tree: parse_opt(&state.new_tree)?,
            checkout_onto: parse_opt(&state.checkout_onto)?,
        },
    })
}

fn stored_rewrites(state: &StoredExec) -> Result<ExecRewrites, HistoryError> {
    Ok(ExecRewrites {
        planned: state
            .planned
            .iter()
            .map(|hex| hex.as_deref().map(ObjectId::from_hex).transpose())
            .collect::<Result<_, _>>()?,
        remap: state
            .remap
            .iter()
            .map(|(k, v)| Ok((ObjectId::from_hex(k)?, ObjectId::from_hex(v)?)))
            .collect::<Result<_, HistoryError>>()?,
    })
}

/// Resume only after the caller has explicitly saved or removed the exec's edits, or
/// resolved and staged a pending replay conflict.
///
/// Later replays and resolved commits use the rewrite options the rebase started with.
pub(super) fn resume_exec_rebase(
    repo: &Repository,
    apply: &super::ApplyOptions,
    skip: bool,
    progress: &mut dyn crate::progress::ProgressSink,
    cancel: &dyn crate::progress::Cancel,
) -> Result<super::RebaseOutcome, super::RebaseSessionError> {
    let state = load_exec_stop(repo)?;
    let rewrite = &state.rewrite;
    let prestate = stored_prestate(&state)?;
    let mut plan = stored_plan(repo, &state, &prestate)?;
    let mut rewrites = stored_rewrites(&state)?;
    if rewrites.planned.len() != plan.commits.len() || state.row >= plan.commits.len() {
        return Err(exec_io("invalid exec replay position").into());
    }
    ensure_exec_plan_current(repo, &prestate, &plan)?;
    let expected = ObjectId::from_hex(&state.head).map_err(HistoryError::from)?;
    let head = resolve_head_for_repository(repo).map_err(exec_io)?;
    if head != (HeadState::Detached { oid: expected }) {
        let still_on_branch = match &prestate.head {
            SavedHead::Symbolic(_) => head.oid() == Some(&prestate.head_commit),
            SavedHead::Detached(_) => false,
        };
        if still_on_branch {
            let tip_tree = commit_tree_for_apply(repo, expected)?;
            update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &expected)
                .map_err(exec_io)?;
            checkout_exec_tree_transition(repo, None, &tip_tree)
                .map_err(super::RebaseSessionError::History)?;
        } else {
            return Err(super::RebaseSessionError::ExecHeadChanged { expected });
        }
    }
    let row = state.row;
    let (cursor, current_tree) = if state.pending_pick {
        let planned = rewrites.planned[row].ok_or_else(|| exec_io("invalid exec replay row"))?;
        if skip {
            let tree = commit_tree_for_apply(repo, expected)?;
            checkout_hard_to_tree_from_head_tree(repo, &tree, &tree, progress, cancel)?;
            let entry = &mut plan.commits[row];
            entry.new = None;
            entry.change_id = None;
            rewrites.remap.insert(planned, expected);
            (
                ExecCursor {
                    row,
                    command: 0,
                    head: Some(expected),
                },
                tree,
            )
        } else {
            let mut index = repo.load_index()?;
            if index.has_unmerged_entries() {
                return Err(super::RebaseSessionError::UnmergedPaths);
            }
            let tree = crate::write_tree::write_tree_update_index(
                repo.odb(),
                &mut index,
                "",
                crate::write_tree::WriteTreeFlags::silent(),
            )?;
            if tree == commit_tree_for_apply(repo, expected)? {
                return Err(super::RebaseSessionError::NothingToCommit);
            }
            let resolved = write_resolved_replay(repo, planned, tree, expected, rewrite)?;
            set_row_replacement(repo, &mut plan.commits[row], resolved)?;
            rewrites.remap.insert(planned, resolved);
            (
                ExecCursor {
                    row,
                    command: 0,
                    head: None,
                },
                tree,
            )
        }
    } else {
        if let Err(err) = ensure_worktree_clean_for_rebase_exec(repo) {
            return Err(match err {
                Error::DirtyWorktree { paths, .. } => HistoryError::ExecDirty {
                    commit: expected,
                    command: state
                        .commands
                        .get(state.next_command.saturating_sub(1))
                        .cloned()
                        .ok_or_else(|| exec_io("invalid exec command position"))?,
                    paths,
                    runs: Vec::new(),
                },
                other => exec_io(other),
            }
            .into());
        }
        let command = if skip {
            state.commands.len()
        } else {
            state.next_command
        };
        (
            ExecCursor {
                row,
                command,
                head: Some(expected),
            },
            commit_tree_for_apply(repo, expected)?,
        )
    };
    let spec = ExecSpec {
        commands: state.commands.clone(),
    };
    let mut session = ExecDetachedSession {
        repo,
        prestate: &prestate,
        current_tree,
        active: true,
    };
    let exec = match execute_plan(
        &mut session,
        &mut plan,
        &mut rewrites,
        &spec,
        rewrite,
        repo.command_runner().as_ref(),
        cursor,
    ) {
        Ok(exec) => exec,
        // These errors are returned only after a successful rollback. The saved stop
        // no longer describes HEAD, so finish cancelling that session too.
        Err(
            err @ (HistoryError::ExecFailed { .. }
            | HistoryError::ExecHeadMoved { .. }
            | HistoryError::Conflict { .. }
            | HistoryError::BecameEmpty { .. }),
        ) => {
            super::rebase_session::clear_rebase_state(repo.git_dir())?;
            return Err(err.into());
        }
        Err(err) => return Err(err.into()),
    };
    // Restore the exact pre-exec HEAD/index before applying the updated atomic plan.
    // Keep the stop state until apply succeeds, so ref failures remain recoverable.
    let applied = super::apply_rewrite(repo, &plan, apply, progress, cancel)?;
    super::rebase_session::clear_rebase_state(repo.git_dir())?;
    Ok(super::RebaseOutcome::ExecDone {
        applied,
        plan: Box::new(plan),
        exec,
    })
}

/// Refuse to resume when the rebased branch or a ref the stored plan moves no longer has the
/// tip the plan was built from; replaying would only fail at the final compare-and-swap.
fn ensure_exec_plan_current(
    repo: &Repository,
    prestate: &ExecPrestate,
    plan: &RewritePlan,
) -> Result<(), super::RebaseSessionError> {
    let branch = match &prestate.head {
        SavedHead::Symbolic(name) => Some((name.as_str(), prestate.head_commit)),
        SavedHead::Detached(_) => None,
    };
    let planned = plan
        .ref_updates
        .iter()
        .filter(|update| update.name != "HEAD")
        .map(|update| (update.name.as_str(), update.old));
    for (name, expected) in branch.into_iter().chain(planned) {
        let actual = repo.references().resolve_if_exists(name)?;
        if actual != Some(expected) {
            return Err(super::RebaseSessionError::StaleExecPlan {
                name: name.to_owned(),
                expected,
                actual,
            });
        }
    }
    Ok(())
}

/// Point the rebased branch back at its pre-rebase tip when something moved it during the
/// stop, swapping only from the value just read. Returns the overwritten movement, if any.
fn reset_moved_exec_branch(
    repo: &Repository,
    prestate: &ExecPrestate,
    identity: &crate::ident::Signature,
) -> Result<Option<super::PlannedRefUpdate>, super::RebaseSessionError> {
    use crate::refs::store::{Expected, RawRef, RefStoreError, RefUpdate, RefUpdateFlags};
    let SavedHead::Symbolic(name) = &prestate.head else {
        return Ok(None);
    };
    let current = repo.references().resolve_if_exists(name)?;
    if current == Some(prestate.head_commit) {
        return Ok(None);
    }
    let old = current.unwrap_or_else(crate::diff::zero_oid);
    let update = RefUpdate {
        name: name.clone(),
        new_value: Some(RawRef::Direct(prestate.head_commit)),
        expected: current.map_or(Expected::Missing, Expected::Oid),
        reflog: Some(crate::refs::store::ReflogUpdate {
            identity: identity.reflog_name_email(),
            message: "rebase (abort)".into(),
            time: identity.time,
            old_oid: Some(old),
            new_oid: Some(prestate.head_commit),
        }),
        flags: RefUpdateFlags {
            no_deref: true,
            ..RefUpdateFlags::default()
        },
    };
    match crate::refs::commit_ref_update_on_store(repo.refs(), update) {
        Ok(()) => Ok(Some(super::PlannedRefUpdate {
            name: name.clone(),
            old,
            new: prestate.head_commit,
        })),
        Err(Error::RefStore(RefStoreError::ExpectedMismatch { .. })) => {
            Err(super::RebaseSessionError::AbortRefMoved { name: name.clone() })
        }
        Err(err) => Err(err.into()),
    }
}

pub(super) fn abort_exec_rebase(
    repo: &Repository,
    identity: &crate::ident::Signature,
) -> Result<super::RebaseAbort, super::RebaseSessionError> {
    let state = load_exec_stop(repo)?;
    let prestate = stored_prestate(&state)?;
    let head = resolve_head_for_repository(repo).map_err(exec_io)?;
    let current = head
        .oid()
        .copied()
        .ok_or_else(|| exec_io("exec replay HEAD is unreadable"))?;
    let current_tree = commit_tree_for_apply(repo, current)?;
    let overwritten = reset_moved_exec_branch(repo, &prestate, identity)?;
    restore_exec_prestate(repo, &prestate, current_tree).map_err(|err| match err {
        HistoryError::RefCasMismatch { name } => super::RebaseSessionError::AbortRefMoved { name },
        other => other.into(),
    })?;
    super::rebase_session::clear_rebase_state(repo.git_dir())?;
    Ok(super::RebaseAbort {
        overwritten: overwritten.into_iter().collect(),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    use std::io;
    use std::sync::Arc;

    use crate::command_runner::{
        system_command_runner, CommandRunner, CommandSpec, RecordedResponse, RecordingRunner,
        RunningCommand,
    };
    use crate::history::{
        plan_rewrite, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
    };
    use crate::index::write_tree::write_tree_from_index;
    use crate::objects::parse_commit;
    use crate::objects::{serialize_commit, CommitData, ObjectKind};
    use crate::porcelain::status::{status, StatusOptions};
    use crate::progress::{NeverCancel, NullProgress};
    use crate::refs::{resolve_ref, write_ref, write_symbolic_ref};
    use crate::repo::{InitOptions, Repository};

    use super::*;

    #[derive(Debug)]
    struct SpawnFailRunner;

    impl CommandRunner for SpawnFailRunner {
        fn spawn(&self, _spec: &CommandSpec) -> io::Result<RunningCommand> {
            Err(io::Error::other("spawn failed"))
        }
    }

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
    }

    fn index_sha256(repo: &Repository) -> u64 {
        let bytes = fs::read(repo.index_path()).expect("index");
        let mut hasher = DefaultHasher::new();
        bytes.hash(&mut hasher);
        hasher.finish()
    }

    fn commit_file(
        repo: &Repository,
        branch: &str,
        path: &str,
        contents: &[u8],
        message: &str,
        parent: Option<ObjectId>,
    ) -> ObjectId {
        let worktree = repo.work_tree().expect("worktree");
        let file_path = worktree.join(path);
        if let Some(parent_dir) = file_path.parent() {
            std::fs::create_dir_all(parent_dir).expect("mkdir");
        }
        std::fs::write(&file_path, contents).expect("write");
        let mut index = repo.load_index().expect("index");
        let blob = repo.odb().write(ObjectKind::Blob, contents).expect("blob");
        index.add_or_replace(crate::index::IndexEntry {
            ctime_sec: 0,
            ctime_nsec: 0,
            mtime_sec: 0,
            mtime_nsec: 0,
            dev: 0,
            ino: 0,
            mode: crate::index::MODE_REGULAR,
            uid: 0,
            gid: 0,
            size: contents.len() as u32,
            oid: blob,
            flags: ((path.len() as u16) << 8) | 0,
            flags_extended: None,
            path: path.as_bytes().to_vec(),
            base_index_pos: 0,
        });
        let tree = write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        let parents = parent.into_iter().collect();
        let commit = serialize_commit(&CommitData {
            tree,
            parents,
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: message.into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let oid = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        write_ref(repo.git_dir(), &format!("refs/heads/{branch}"), &oid).expect("ref");
        write_symbolic_ref(repo.git_dir(), "HEAD", &format!("refs/heads/{branch}")).expect("head");
        oid
    }

    fn remove_path_from_index_and_commit(
        repo: &Repository,
        branch: &str,
        path: &str,
        message: &str,
        parent: ObjectId,
    ) -> ObjectId {
        let worktree = repo.work_tree().expect("worktree");
        let file_path = worktree.join(path);
        let _ = std::fs::remove_file(&file_path);
        let mut index = repo.load_index().expect("index");
        index.remove(path.as_bytes());
        let tree = write_tree_from_index(repo.odb(), &index, "").expect("tree");
        repo.write_index(&mut index).expect("write index");
        let commit = serialize_commit(&CommitData {
            tree,
            parents: vec![parent],
            author: "T <t@t> 0 +0000".into(),
            committer: "T <t@t> 0 +0000".into(),
            author_raw: vec![],
            committer_raw: vec![],
            encoding: None,
            message: message.into(),
            raw_message: None,
            extra_headers: vec![],
        });
        let oid = repo
            .odb()
            .write(ObjectKind::Commit, &commit)
            .expect("commit");
        write_ref(repo.git_dir(), &format!("refs/heads/{branch}"), &oid).expect("ref");
        write_symbolic_ref(repo.git_dir(), "HEAD", &format!("refs/heads/{branch}")).expect("head");
        oid
    }

    fn two_commit_plan(repo: &Repository, root: ObjectId, tip: ObjectId) -> RewritePlan {
        let root_tree = parse_commit(&repo.odb().read(&root).expect("read").data)
            .expect("parse")
            .tree;
        let tip_tree = parse_commit(&repo.odb().read(&tip).expect("read").data)
            .expect("parse")
            .tree;
        let mut edits = HistoryEdits::new();
        edits.insert(
            root,
            CommitEdit::Rewrite {
                tree: Some(root_tree),
                message: Some("root'\n".into()),
                author: None,
                parents: None,
            },
        );
        edits.insert(
            tip,
            CommitEdit::Rewrite {
                tree: Some(tip_tree),
                message: Some("tip'\n".into()),
                author: None,
                parents: None,
            },
        );
        plan_rewrite(repo, &edits, &test_rewrite()).expect("plan")
    }

    fn test_rewrite() -> RewriteOptions {
        RewriteOptions {
            scope: RefScope::Head,
            empty: EmptyPolicy::Drop,
            committer: "T <t@t> 1 +0000".parse().expect("signature"),
            preserve_committer: false,
            sign: false,
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_restores_one_consistent_head_when_injected_ref_store_disagrees() {
        use crate::config::environment::RepositoryOptions;
        use crate::refs::store::{
            Expected, MemoryRefStore, RawRef, RefStore, RefTransaction, RefUpdate, RefUpdateFlags,
        };
        use crate::repo::RepositoryBuilder;

        let (dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let mut plan = two_commit_plan(&repo, root, tip);

        write_ref(repo.git_dir(), "refs/heads/main", &root).expect("stale disk main");
        let disk_status = || grit_test_support::git(dir.path(), &["status", "--porcelain"]);
        assert_eq!(
            disk_status(),
            "A  b.txt\n",
            "disk HEAD is stale behind the index"
        );
        let index_before = index_sha256(&repo);

        let store = Arc::new(MemoryRefStore::new());
        let txn = RefTransaction::new()
            .update(RefUpdate {
                name: "HEAD".to_owned(),
                new_value: Some(RawRef::Direct(tip)),
                expected: Expected::Any,
                reflog: None,
                flags: RefUpdateFlags::default(),
            })
            .expect("txn");
        store
            .prepare(txn)
            .expect("prepare")
            .commit()
            .expect("commit");
        let injected = RepositoryBuilder::new(
            RepositoryOptions::empty().with_work_tree(dir.path().to_path_buf()),
        )
        .ref_store(store.clone())
        .open(repo.git_dir(), Some(dir.path()))
        .expect("open injected");
        assert_eq!(injected.head_commit().expect("injected head"), tip);
        let disk_head = fs::read(repo.git_dir().join("HEAD")).expect("disk HEAD");

        let runner = RecordingRunner::always_success();
        let spec = ExecSpec {
            commands: vec!["true".into()],
        };
        verify_rebase_with_exec(
            &injected,
            &mut plan,
            &spec,
            &test_rewrite(),
            runner.as_ref(),
        )
        .expect("exec");

        assert_eq!(
            store.read_raw("HEAD").expect("store HEAD"),
            Some(RawRef::Direct(tip)),
            "restore writes the HEAD the clean check and saved tree came from"
        );
        assert_eq!(index_sha256(&repo), index_before);
        assert_eq!(fs::read(dir.path().join("b.txt")).expect("b.txt"), b"2\n");
        assert_eq!(
            fs::read(repo.git_dir().join("HEAD")).expect("disk HEAD"),
            disk_head,
            "HEAD moves only in the injected store"
        );
        assert_eq!(
            grit_test_support::git(dir.path(), &["rev-parse", "HEAD"]).trim(),
            root.to_hex()
        );
        assert_eq!(disk_status(), "A  b.txt\n", "on-disk refs stay untouched");
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_success_preserves_tracked_edits_before_next_command() {
        for staged in [false, true] {
            let (_dir, repo) = init_repo();
            let root = commit_file(&repo, "main", "a.txt", b"base\n", "root\n", None);
            let tip = commit_file(&repo, "main", "b.txt", b"one\n", "tip\n", Some(root));
            let mut plan = two_commit_plan(&repo, root, tip);
            let command = if staged {
                "echo hook-edit >> a.txt; git add a.txt"
            } else {
                "echo hook-edit >> a.txt"
            };
            let result = verify_rebase_with_exec(
                &repo,
                &mut plan,
                &ExecSpec {
                    commands: vec![command.into(), "touch should-not-run".into()],
                },
                &test_rewrite(),
                system_command_runner().as_ref(),
            );
            assert!(result.is_err(), "successful exec edits must stop replay");
            assert_eq!(
                fs::read(repo.work_tree().expect("wt").join("a.txt")).expect("edit"),
                b"base\nhook-edit\n"
            );
            assert_eq!(
                resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
                tip
            );
            assert!(!repo
                .work_tree()
                .expect("wt")
                .join("should-not-run")
                .exists());
        }
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_runs_once_per_rewritten_commit_in_order() {
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let mut plan = two_commit_plan(&repo, root, tip);
        let rewritten: Vec<ObjectId> = plan.commits.iter().filter_map(|r| r.new).collect();
        assert_eq!(rewritten.len(), 2);

        let runner = RecordingRunner::always_success();
        let spec = ExecSpec {
            commands: vec!["echo one".into(), "echo two".into()],
        };
        let report =
            verify_rebase_with_exec(&repo, &mut plan, &spec, &test_rewrite(), runner.as_ref())
                .expect("exec");
        assert_eq!(report.runs.len(), 4);
        assert_eq!(report.runs[0].commit, rewritten[0]);
        assert_eq!(report.runs[1].commit, rewritten[0]);
        assert_eq!(report.runs[2].commit, rewritten[1]);
        assert_eq!(report.runs[3].commit, rewritten[1]);

        assert_eq!(runner.specs().len(), 4);
        assert_eq!(resolve_ref(repo.git_dir(), "HEAD").expect("head"), tip);
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_failure_restores_everything_and_reports_commit() {
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let worktree = repo.work_tree().expect("worktree");
        let a_path = worktree.join("a.txt");
        let a_contents = fs::read(&a_path).expect("a.txt");
        let mut index = repo.load_index().expect("index");
        if let Some(entry) = index.get_mut(b"a.txt", 0) {
            entry.set_assume_unchanged(true);
        }
        repo.write_index(&mut index).expect("write index");
        let index_before = fs::read(repo.index_path()).expect("index bytes");
        let index_hash_before = index_sha256(&repo);

        let main_before = resolve_ref(repo.git_dir(), "refs/heads/main").expect("main");
        let mut plan = two_commit_plan(&repo, root, tip);
        let rewritten: Vec<ObjectId> = plan.commits.iter().filter_map(|r| r.new).collect();

        let runner = RecordingRunner::always_success();
        runner.push_response(RecordedResponse::Success);
        runner.push_response(RecordedResponse::Exit(9));

        let spec = ExecSpec {
            commands: vec!["false".into()],
        };
        let err =
            verify_rebase_with_exec(&repo, &mut plan, &spec, &test_rewrite(), runner.as_ref())
                .expect_err("fail");
        match err {
            HistoryError::ExecFailed {
                commit, exit_code, ..
            } => {
                assert_eq!(commit, rewritten[1]);
                assert_eq!(exit_code, 9);
            }
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            main_before
        );
        assert_eq!(resolve_ref(repo.git_dir(), "HEAD").expect("head"), tip);
        assert_eq!(fs::read(repo.index_path()).expect("index"), index_before);
        assert_eq!(index_sha256(&repo), index_hash_before);
        let restored = repo.load_index().expect("load index");
        let a_entry = restored.get(b"a.txt", 0).expect("a.txt still tracked");
        assert!(a_entry.assume_unchanged());
        assert_eq!(fs::read(&a_path).expect("worktree a.txt"), a_contents);
        assert!(
            !repo.git_dir().join("rebase-merge").exists(),
            "failed exec must not leave recovery state"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_runner_error_restores_head_index_and_worktree() {
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let head_before = resolve_ref(repo.git_dir(), "HEAD").expect("head");
        let main_before = resolve_ref(repo.git_dir(), "refs/heads/main").expect("main");
        let index_before = fs::read(repo.index_path()).expect("index");
        let a_before = fs::read(repo.work_tree().expect("wt").join("a.txt")).expect("a.txt");
        let mut plan = two_commit_plan(&repo, root, tip);
        let runner = Arc::new(SpawnFailRunner);
        let err = verify_rebase_with_exec(
            &repo,
            &mut plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
            &test_rewrite(),
            runner.as_ref(),
        )
        .expect_err("spawn error");
        assert!(matches!(err, HistoryError::WorktreeApply { .. }));
        assert_eq!(
            resolve_ref(repo.git_dir(), "HEAD").expect("head"),
            head_before
        );
        assert_eq!(
            resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            main_before
        );
        assert_eq!(fs::read(repo.index_path()).expect("index"), index_before);
        assert_eq!(
            fs::read(repo.work_tree().expect("wt").join("a.txt")).expect("a.txt"),
            a_before
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_refuses_checkout_that_overwrites_untracked() {
        let (dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "base.txt", b"base\n", "root\n", None);
        let mid = commit_file(
            &repo,
            "main",
            "collision.txt",
            b"tracked\n",
            "add collision\n",
            Some(root),
        );
        let tip = remove_path_from_index_and_commit(
            &repo,
            "main",
            "collision.txt",
            "drop collision\n",
            mid,
        );
        let _tip = tip;
        let collision = dir.path().join("collision.txt");
        std::fs::write(&collision, b"precious untracked").expect("untracked collision");
        let mut plan = two_commit_plan(&repo, root, mid);
        let runner = RecordingRunner::always_success();
        let err = verify_rebase_with_exec(
            &repo,
            &mut plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
            &test_rewrite(),
            runner.as_ref(),
        )
        .expect_err("must not clobber untracked");
        assert!(matches!(
            err,
            HistoryError::LocalChangesWouldBeOverwritten { .. }
        ));
        assert_eq!(
            fs::read(&collision).expect("untracked survives"),
            b"precious untracked"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_failure_restores_worktree_when_exec_dirties_unchanged_blob() {
        let (_dir, repo) = init_repo();
        let stable = b"stable\n";
        let root = commit_file(&repo, "main", "stable.txt", stable, "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let stable_path = repo.work_tree().expect("worktree").join("stable.txt");
        let stable_before = fs::read(&stable_path).expect("stable.txt");
        let mut plan = two_commit_plan(&repo, root, tip);
        let runner = system_command_runner();
        let spec = ExecSpec {
            commands: vec!["printf 'dirty\\n' >> stable.txt; exit 7".into()],
        };
        let err =
            verify_rebase_with_exec(&repo, &mut plan, &spec, &test_rewrite(), runner.as_ref())
                .expect_err("exec fail");
        match err {
            HistoryError::ExecFailed { exit_code, .. } => assert_eq!(exit_code, 7),
            other => panic!("unexpected {other:?}"),
        }
        assert_eq!(
            fs::read(&stable_path).expect("stable.txt after restore"),
            stable_before
        );
        let model = status(
            &repo,
            &StatusOptions::default(),
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("status");
        assert!(
            model.staged.is_empty() && model.unstaged.is_empty(),
            "worktree must be clean after exec rollback, staged={:?} unstaged={:?}",
            model.staged,
            model.unstaged
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_detects_same_size_racy_edit_with_unchanged_stat_fields() {
        struct RacyEditRunner<'a> {
            repo: &'a Repository,
            inner: Arc<dyn CommandRunner>,
        }
        impl CommandRunner for RacyEditRunner<'_> {
            fn spawn(&self, spec: &CommandSpec) -> io::Result<RunningCommand> {
                let path = self.repo.work_tree().expect("worktree").join("a.txt");
                let stamp = filetime::FileTime::from_unix_time(1_700_000_000, 0);
                filetime::set_file_mtime(&path, stamp)?;
                let mut index = self.repo.load_index().expect("reload index");
                let entry = index.get_mut(b"a.txt", 0).expect("entry");
                entry.mtime_sec = 1_700_000_000;
                entry.mtime_nsec = 0;
                self.repo.write_index(&mut index).expect("write index");
                filetime::set_file_mtime(&self.repo.index_path(), stamp)?;
                fs::write(&path, b"X\n")?;
                filetime::set_file_mtime(&path, stamp)?;
                self.inner.spawn(spec)
            }
        }
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let mut cfg = fs::OpenOptions::new()
            .append(true)
            .open(repo.git_dir().join("config"))
            .expect("config");
        use std::io::Write;
        writeln!(cfg, "[core]\ntrustctime = false").expect("config");
        let mut plan = two_commit_plan(&repo, root, tip);
        let repo = Repository::open(
            repo.git_dir(),
            &crate::environment::RepositoryOptions::empty()
                .with_work_tree(repo.work_tree().expect("wt")),
        )
        .expect("reopen with trustctime disabled");
        let runner = RacyEditRunner {
            repo: &repo,
            inner: RecordingRunner::always_success(),
        };
        let err = verify_rebase_with_exec(
            &repo,
            &mut plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
            &test_rewrite(),
            &runner,
        )
        .expect_err("racy edit");
        assert!(matches!(err, HistoryError::ExecDirty { paths, .. } if paths == ["a.txt"]));
        assert_eq!(
            fs::read(repo.work_tree().expect("wt").join("a.txt")).expect("edit"),
            b"X\n"
        );
        assert_eq!(
            resolve_ref(repo.git_dir(), "refs/heads/main").expect("branch"),
            tip
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_amend_of_unchanged_head_commit_adds_its_ref_update() {
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let mut edits = HistoryEdits::new();
        edits.insert(
            tip,
            CommitEdit::Reuse {
                existing: tip,
                parent: tip,
            },
        );
        let mut plan = plan_rewrite(&repo, &edits, &test_rewrite()).expect("plan");
        assert!(plan.ref_updates.is_empty(), "reusing the tip moves no ref");
        let spec = ExecSpec {
            commands: vec!["env GIT_CONFIG_GLOBAL=/dev/null GIT_AUTHOR_NAME=T \
                GIT_AUTHOR_EMAIL=t@t GIT_COMMITTER_NAME=T GIT_COMMITTER_EMAIL=t@t \
                git commit -q --amend -m amended"
                .into()],
        };
        verify_rebase_with_exec(
            &repo,
            &mut plan,
            &spec,
            &test_rewrite(),
            system_command_runner().as_ref(),
        )
        .expect("exec");
        let [update] = plan.ref_updates.as_slice() else {
            panic!("expected one ref update, got {:?}", plan.ref_updates)
        };
        assert_eq!(update.name, "refs/heads/main");
        assert_eq!(update.old, tip);
        assert_eq!(plan.commits[0].new, Some(update.new));
        assert_eq!(plan.commits[0].subject.as_deref(), Some("amended"));
        assert!(plan.head.commit_changes);
        assert_eq!(
            resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            tip
        );
        crate::history::apply_rewrite(
            &repo,
            &plan,
            &crate::history::ApplyOptions {
                identity: "T <t@t> 2 +0000".parse().expect("identity"),
                reflog_message: "rebase".into(),
                record_operation: None,
                hooks: crate::PostOperationHooks::Skip,
            },
            &mut NullProgress,
            &NeverCancel,
        )
        .expect("apply");
        assert_eq!(
            resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
            update.new
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_checkpoints_grit_exec_while_command_is_in_flight() {
        use std::sync::mpsc;
        use std::thread;
        use std::time::Duration;

        struct PausedRunner {
            started: mpsc::Sender<()>,
            release: Arc<std::sync::Mutex<mpsc::Receiver<()>>>,
        }

        impl CommandRunner for PausedRunner {
            fn spawn(&self, spec: &CommandSpec) -> io::Result<RunningCommand> {
                self.started.send(()).ok();
                self.release
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .recv()
                    .ok();
                RecordingRunner::always_success().spawn(spec)
            }
        }

        let (dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let main_before = resolve_ref(repo.git_dir(), "refs/heads/main").expect("main");
        let tip_tree = parse_commit(&repo.odb().read(&tip).expect("read").data)
            .expect("parse")
            .tree;
        let mut edits = HistoryEdits::new();
        edits.insert(
            tip,
            CommitEdit::Rewrite {
                tree: Some(tip_tree),
                message: Some("tip'\n".into()),
                author: None,
                parents: None,
            },
        );
        let mut plan = plan_rewrite(&repo, &edits, &test_rewrite()).expect("plan");
        let (started_tx, started_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let runner = Arc::new(PausedRunner {
            started: started_tx,
            release: Arc::new(std::sync::Mutex::new(release_rx)),
        });
        let merge = dir.path().join(".git/rebase-merge");
        let spec = ExecSpec {
            commands: vec!["true".into()],
        };
        thread::scope(|scope| {
            let handle = scope.spawn(|| {
                verify_rebase_with_exec(&repo, &mut plan, &spec, &test_rewrite(), runner.as_ref())
            });
            started_rx
                .recv_timeout(Duration::from_secs(10))
                .expect("exec command should start");
            assert!(merge.join("grit-exec").is_file(), "grit-exec must exist");
            assert_ne!(
                resolve_ref(repo.git_dir(), "HEAD").expect("head"),
                tip,
                "HEAD should be detached at the replay tip while the command runs"
            );
            assert_eq!(
                resolve_ref(repo.git_dir(), "refs/heads/main").expect("main"),
                main_before,
                "branch ref must stay at the pre-rebase tip during exec"
            );
            release_tx.send(()).expect("release command");
            handle.join().expect("join")
        })
        .expect("exec completes");
        assert!(
            !merge.exists(),
            "successful exec must clear rebase-merge after restore"
        );
    }

    #[cfg_attr(
        not(feature = "full-tests"),
        ignore = "starts system git or is heavy; run with --features full-tests"
    )]
    #[test]
    fn exec_refuses_dirty_worktree() {
        let (dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        std::fs::write(dir.path().join("a.txt"), "dirty\n").expect("dirty");
        let mut plan = two_commit_plan(&repo, root, tip);
        let runner = RecordingRunner::always_success();
        let err = verify_rebase_with_exec(
            &repo,
            &mut plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
            &test_rewrite(),
            runner.as_ref(),
        )
        .expect_err("dirty");
        assert!(matches!(err, HistoryError::UncommittedChanges));
    }
}
