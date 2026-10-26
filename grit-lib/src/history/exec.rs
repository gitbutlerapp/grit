//! Run `rebase --exec` hooks after planning and before ref updates.

use std::fs;
use std::path::Path;

use crate::command_runner::{
    run_sh_dash_c, CommandOutput, CommandRunner, CommandStdin, CommandStdio,
};
use crate::error::Error;
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use crate::porcelain::switch::{
    checkout_tree_transition_allow_local, verify_tree_transition_allow_local,
    TreeTransitionVerifyError,
};
use crate::porcelain::worktree_guard::{
    ensure_worktree_clean_for_rebase_exec, ensure_worktree_clean_from_snapshot,
    worktree_snapshot_with_head_tree,
};
use crate::refs::{update_direct_ref_on_store, write_symbolic_ref_on_store};
use crate::repo::Repository;
use crate::state::{resolve_head_for_repository, HeadState};

use super::apply::commit_tree_for_apply;
use super::error::HistoryError;
use super::types::RewritePlan;

/// Shell commands to run at each replayed commit during `rebase --exec`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ExecSpec {
    /// Commands executed in order at every rewritten commit (`sh -c`).
    pub commands: Vec<String>,
}

/// One `--exec` invocation at a specific commit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRunRecord {
    /// Commit checked out when the command ran.
    pub commit: ObjectId,
    /// Shell command string.
    pub command: String,
    /// Subprocess exit code (`1` when the OS provides no code).
    pub exit_code: i32,
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
}

/// Successful exec invocations captured during one verification or recovery call.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
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
        Ok(()) => err,
        Err(restore_err) => restore_err,
    }
}

/// Checkout each rewritten commit in plan order, run `spec.commands`, then restore `HEAD`,
/// the index, and the worktree on clean completion. Branch refs are not modified.
///
/// # Errors
///
/// - [`HistoryError::BareRepository`] in a bare repository.
/// - [`HistoryError::UncommittedChanges`] when the worktree or index is dirty.
/// - [`HistoryError::LocalChangesWouldBeOverwritten`] when a checkout would clobber local paths.
/// - [`HistoryError::ExecFailed`] when a command exits non-zero (state is rolled back).
/// - [`HistoryError::ExecDirty`] when a successful command changes tracked paths. The
///   edited index and worktree remain at the detached replay commit, branch refs stay
///   unchanged, and exec recovery state is saved in `rebase-merge/grit-exec`.
///   Save or remove the edits before [`super::continue_rebase`] or [`super::skip_rebase`];
///   [`super::abort_rebase`] explicitly discards them and restores the original state.
/// - I/O, checkout, or subprocess failures.
pub fn verify_rebase_with_exec(
    repo: &Repository,
    plan: &RewritePlan,
    spec: &ExecSpec,
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

    execute_plan(&mut session, plan, spec, runner, 0, 0)
}

fn execute_plan(
    session: &mut ExecDetachedSession<'_>,
    plan: &RewritePlan,
    spec: &ExecSpec,
    runner: &dyn CommandRunner,
    start_commit: usize,
    start_command: usize,
) -> Result<ExecReport, HistoryError> {
    let repo = session.repo;
    let mut runs = Vec::new();
    let worktree = repo.work_tree().ok_or(HistoryError::BareRepository)?;
    let commits: Vec<_> = plan.commits.iter().filter_map(|row| row.new).collect();
    for (commit_index, commit) in commits.into_iter().enumerate().skip(start_commit) {
        let commit_tree = commit_tree_for_apply(repo, commit)?;
        update_direct_ref_on_store(repo.refs(), repo.git_dir(), "HEAD", &commit).map_err(|e| {
            HistoryError::WorktreeApply {
                message: e.to_string(),
            }
        })?;
        session.active = true;

        if let Err(err) =
            checkout_exec_tree_transition(repo, Some(&session.current_tree), &commit_tree)
        {
            return Err(fail_exec_session(session, err));
        }
        session.current_tree = commit_tree;

        let first_command = if commit_index == start_commit {
            start_command
        } else {
            0
        };
        for (command_index, command) in spec.commands.iter().enumerate().skip(first_command) {
            let output = match run_exec_command(runner, command, worktree) {
                Ok(output) => output,
                Err(err) => return Err(fail_exec_session(session, err)),
            };
            let exit_code = output.status.code.unwrap_or(1);
            let stdout = output.stdout;
            let stderr = output.stderr;
            runs.push(ExecRunRecord {
                commit,
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
                    commit,
                    exit_code,
                    stdout,
                    stderr,
                    completed_runs,
                };
                return Err(fail_exec_session(session, exec_err));
            }
            // Reload the subprocess-written index and compare against the replay tree,
            // including racily clean worktree entries, before any further mutation.
            let clean = worktree_snapshot_with_head_tree(repo, Some(commit_tree)).and_then(
                |mut snapshot| {
                    ensure_worktree_clean_from_snapshot(
                        repo,
                        &mut snapshot,
                        crate::error::WorktreeOperation::RebaseExec,
                        false,
                    )
                },
            );
            if let Err(err) = clean {
                session.active = false;
                store_exec_stop(session, plan, spec, commit_index, command_index + 1)?;
                return Err(match err {
                    Error::DirtyWorktree { paths, .. } => HistoryError::ExecDirty {
                        commit,
                        command: command.clone(),
                        paths,
                        runs,
                    },
                    other => exec_io(other),
                });
            }
        }
    }

    session.restore()?;
    Ok(ExecReport { runs })
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

// The exec stop is deliberately distinct from a conflicted pick: the current commit
// is already replayed, and continuing must never commit its index as a resolution.
#[derive(serde::Serialize, serde::Deserialize)]
struct StoredExec {
    head_ref: Option<String>,
    original_head: String,
    original_tree: String,
    index: Vec<u8>,
    commits: Vec<(String, Option<String>, bool)>,
    refs: Vec<(String, String, String)>,
    checkout_onto: Option<String>,
    new_tree: Option<String>,
    commit_changes: bool,
    commands: Vec<String>,
    commit_index: usize,
    next_command: usize,
}

fn exec_io(err: impl std::fmt::Display) -> HistoryError {
    HistoryError::WorktreeApply {
        message: err.to_string(),
    }
}

fn store_exec_stop(
    session: &ExecDetachedSession<'_>,
    plan: &RewritePlan,
    spec: &ExecSpec,
    commit_index: usize,
    next_command: usize,
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
        refs: plan
            .ref_updates
            .iter()
            .map(|r| (r.name.clone(), r.old.to_hex(), r.new.to_hex()))
            .collect(),
        checkout_onto: plan.head.checkout_onto.map(|o| o.to_hex()),
        new_tree: plan.head.new_tree.map(|o| o.to_hex()),
        commit_changes: plan.head.commit_changes,
        commands: spec.commands.clone(),
        commit_index,
        next_command,
    };
    let dir = session.repo.git_dir().join("rebase-merge");
    fs::create_dir_all(&dir).map_err(exec_io)?;
    let mut lock = crate::lockfile::LockFile::acquire(
        &dir.join("grit-exec.lock"),
        crate::lockfile::LockWait::Immediately,
    )
    .map_err(exec_io)?;
    lock.write_all(&serde_json::to_vec(&state).map_err(exec_io)?)
        .map_err(exec_io)?;
    lock.commit_to(&dir.join("grit-exec")).map_err(exec_io)?;
    fs::write(dir.join("orig-head"), format!("{}\n", state.original_head)).map_err(exec_io)?;
    fs::write(
        dir.join("head-name"),
        format!("{}\n", state.head_ref.as_deref().unwrap_or("detached HEAD")),
    )
    .map_err(exec_io)?;
    Ok(())
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

/// Resume only after the caller has explicitly saved or removed the exec's edits.
pub(super) fn resume_exec_rebase(
    repo: &Repository,
    apply: &super::ApplyOptions,
    skip: bool,
) -> Result<super::RebaseOutcome, super::RebaseSessionError> {
    use super::{HeadEffect, PlannedRefUpdate, RewrittenCommit};
    let state = load_exec_stop(repo)?;
    let prestate = stored_prestate(&state)?;
    let plan = RewritePlan {
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
            new_tree: state
                .new_tree
                .as_deref()
                .map(ObjectId::from_hex)
                .transpose()
                .map_err(HistoryError::from)?,
            checkout_onto: state
                .checkout_onto
                .as_deref()
                .map(ObjectId::from_hex)
                .transpose()
                .map_err(HistoryError::from)?,
        },
    };
    let commits: Vec<_> = plan.commits.iter().filter_map(|r| r.new).collect();
    let commit = *commits
        .get(state.commit_index)
        .ok_or_else(|| exec_io("invalid exec replay position"))?;
    let head = resolve_head_for_repository(repo).map_err(exec_io)?;
    if head != (HeadState::Detached { oid: commit }) {
        return Err(super::RebaseSessionError::ExecHeadChanged { expected: commit });
    }
    if let Err(err) = ensure_worktree_clean_for_rebase_exec(repo) {
        return Err(match err {
            Error::DirtyWorktree { paths, .. } => HistoryError::ExecDirty {
                commit,
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
    let spec = ExecSpec {
        commands: state.commands,
    };
    let next_command = if skip {
        spec.commands.len()
    } else {
        state.next_command
    };
    let mut session = ExecDetachedSession {
        repo,
        prestate: &prestate,
        current_tree: commit_tree_for_apply(repo, commit)?,
        active: true,
    };
    let exec = match execute_plan(
        &mut session,
        &plan,
        &spec,
        repo.command_runner().as_ref(),
        state.commit_index,
        next_command,
    ) {
        Ok(exec) => exec,
        // ExecFailed is returned only after a successful rollback. The saved stop
        // no longer describes HEAD, so finish cancelling that session too.
        Err(err @ HistoryError::ExecFailed { .. }) => {
            super::rebase_session::clear_rebase_state(repo.git_dir())?;
            return Err(err.into());
        }
        Err(err) => return Err(err.into()),
    };
    // Restore the exact pre-exec HEAD/index before applying the original atomic plan.
    // Keep the stop state until apply succeeds, so ref failures remain recoverable.
    let applied = super::apply_rewrite(
        repo,
        &plan,
        apply,
        &mut crate::progress::NullProgress,
        &crate::progress::NeverCancel,
    )?;
    super::rebase_session::clear_rebase_state(repo.git_dir())?;
    Ok(super::RebaseOutcome::ExecDone {
        applied,
        plan: Box::new(plan),
        exec,
    })
}

pub(super) fn abort_exec_rebase(repo: &Repository) -> Result<(), super::RebaseSessionError> {
    let state = load_exec_stop(repo)?;
    let prestate = stored_prestate(&state)?;
    let head = resolve_head_for_repository(repo).map_err(exec_io)?;
    let current = head
        .oid()
        .copied()
        .ok_or_else(|| exec_io("exec replay HEAD is unreadable"))?;
    restore_exec_prestate(repo, &prestate, commit_tree_for_apply(repo, current)?)?;
    super::rebase_session::clear_rebase_state(repo.git_dir())
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
        plan_rewrite(
            repo,
            &edits,
            &RewriteOptions {
                scope: RefScope::Head,
                empty: EmptyPolicy::Drop,
                committer: "T <t@t> 1 +0000".parse().expect("signature"),
                preserve_committer: false,
                sign: false,
            },
        )
        .expect("plan")
    }

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
        let plan = two_commit_plan(&repo, root, tip);

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
        verify_rebase_with_exec(&injected, &plan, &spec, runner.as_ref()).expect("exec");

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

    #[test]
    fn exec_success_preserves_tracked_edits_before_next_command() {
        for staged in [false, true] {
            let (_dir, repo) = init_repo();
            let root = commit_file(&repo, "main", "a.txt", b"base\n", "root\n", None);
            let tip = commit_file(&repo, "main", "b.txt", b"one\n", "tip\n", Some(root));
            let plan = two_commit_plan(&repo, root, tip);
            let command = if staged {
                "echo hook-edit >> a.txt; git add a.txt"
            } else {
                "echo hook-edit >> a.txt"
            };
            let result = verify_rebase_with_exec(
                &repo,
                &plan,
                &ExecSpec {
                    commands: vec![command.into(), "touch should-not-run".into()],
                },
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

    #[test]
    fn exec_runs_once_per_rewritten_commit_in_order() {
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let plan = two_commit_plan(&repo, root, tip);
        let rewritten: Vec<ObjectId> = plan.commits.iter().filter_map(|r| r.new).collect();
        assert_eq!(rewritten.len(), 2);

        let runner = RecordingRunner::always_success();
        let spec = ExecSpec {
            commands: vec!["echo one".into(), "echo two".into()],
        };
        let report = verify_rebase_with_exec(&repo, &plan, &spec, runner.as_ref()).expect("exec");
        assert_eq!(report.runs.len(), 4);
        assert_eq!(report.runs[0].commit, rewritten[0]);
        assert_eq!(report.runs[1].commit, rewritten[0]);
        assert_eq!(report.runs[2].commit, rewritten[1]);
        assert_eq!(report.runs[3].commit, rewritten[1]);

        assert_eq!(runner.specs().len(), 4);
        assert_eq!(resolve_ref(repo.git_dir(), "HEAD").expect("head"), tip);
    }

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
        let plan = two_commit_plan(&repo, root, tip);
        let rewritten: Vec<ObjectId> = plan.commits.iter().filter_map(|r| r.new).collect();

        let runner = RecordingRunner::always_success();
        runner.push_response(RecordedResponse::Success);
        runner.push_response(RecordedResponse::Exit(9));

        let spec = ExecSpec {
            commands: vec!["false".into()],
        };
        let err = verify_rebase_with_exec(&repo, &plan, &spec, runner.as_ref()).expect_err("fail");
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
    }

    #[test]
    fn exec_runner_error_restores_head_index_and_worktree() {
        let (_dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let head_before = resolve_ref(repo.git_dir(), "HEAD").expect("head");
        let main_before = resolve_ref(repo.git_dir(), "refs/heads/main").expect("main");
        let index_before = fs::read(repo.index_path()).expect("index");
        let a_before = fs::read(repo.work_tree().expect("wt").join("a.txt")).expect("a.txt");
        let plan = two_commit_plan(&repo, root, tip);
        let runner = Arc::new(SpawnFailRunner);
        let err = verify_rebase_with_exec(
            &repo,
            &plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
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
        let plan = two_commit_plan(&repo, root, mid);
        let runner = RecordingRunner::always_success();
        let err = verify_rebase_with_exec(
            &repo,
            &plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
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

    #[test]
    fn exec_failure_restores_worktree_when_exec_dirties_unchanged_blob() {
        let (_dir, repo) = init_repo();
        let stable = b"stable\n";
        let root = commit_file(&repo, "main", "stable.txt", stable, "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        let stable_path = repo.work_tree().expect("worktree").join("stable.txt");
        let stable_before = fs::read(&stable_path).expect("stable.txt");
        let plan = two_commit_plan(&repo, root, tip);
        let runner = system_command_runner();
        let spec = ExecSpec {
            commands: vec!["printf 'dirty\\n' >> stable.txt; exit 7".into()],
        };
        let err =
            verify_rebase_with_exec(&repo, &plan, &spec, runner.as_ref()).expect_err("exec fail");
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
        let plan = two_commit_plan(&repo, root, tip);
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
            &plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
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

    #[test]
    fn exec_refuses_dirty_worktree() {
        let (dir, repo) = init_repo();
        let root = commit_file(&repo, "main", "a.txt", b"1\n", "root\n", None);
        let tip = commit_file(&repo, "main", "b.txt", b"2\n", "tip\n", Some(root));
        std::fs::write(dir.path().join("a.txt"), "dirty\n").expect("dirty");
        let plan = two_commit_plan(&repo, root, tip);
        let runner = RecordingRunner::always_success();
        let err = verify_rebase_with_exec(
            &repo,
            &plan,
            &ExecSpec {
                commands: vec!["true".into()],
            },
            runner.as_ref(),
        )
        .expect_err("dirty");
        assert!(matches!(err, HistoryError::UncommittedChanges));
    }
}
