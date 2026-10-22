//! Run `rebase --exec` hooks after planning and before ref updates.

use std::fs;
use std::path::Path;

use crate::command_runner::{
    run_sh_dash_c, CommandOutput, CommandRunner, CommandStdin, CommandStdio,
};
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::checkout::checkout_hard_to_tree_from_head_tree;
use crate::porcelain::worktree_guard::ensure_worktree_clean_for_rebase_exec;
use crate::refs::{write_ref, write_symbolic_ref};
use crate::repo::Repository;
use crate::state::{resolve_head, HeadState};

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

/// Successful completion of every `--exec` command (repository restored to pre-exec state).
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
    index_bytes: Vec<u8>,
    head_tree: ObjectId,
}

/// Checkout each rewritten commit in plan order, run `spec.commands`, then restore `HEAD`,
/// the index, and the worktree. Refs are not modified.
///
/// # Errors
///
/// - [`HistoryError::BareRepository`] in a bare repository.
/// - [`HistoryError::UncommittedChanges`] when the worktree or index is dirty.
/// - [`HistoryError::ExecFailed`] when a command exits non-zero (state is rolled back).
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
    ensure_worktree_clean_for_rebase_exec(repo).map_err(map_clean_error)?;

    let prestate = capture_exec_prestate(repo)?;
    let worktree = repo
        .work_tree()
        .ok_or_else(|| HistoryError::WorktreeApply {
            message: "rebase --exec requires a working tree".into(),
        })?;

    let commits: Vec<ObjectId> = plan.commits.iter().filter_map(|row| row.new).collect();
    if commits.is_empty() {
        return Ok(ExecReport::default());
    }

    let mut runs = Vec::new();
    let mut current_tree = prestate.head_tree;

    for commit in commits {
        let commit_tree = commit_tree_for_apply(repo, commit)?;
        write_ref(repo.git_dir(), "HEAD", &commit).map_err(|e| HistoryError::WorktreeApply {
            message: e.to_string(),
        })?;
        checkout_between_trees(repo, Some(&current_tree), &commit_tree).map_err(|e| {
            let _ = restore_exec_prestate(repo, &prestate, current_tree);
            HistoryError::WorktreeApply {
                message: e.to_string(),
            }
        })?;
        current_tree = commit_tree;

        for command in &spec.commands {
            let output = run_exec_command(runner, command, worktree)?;
            let exit_code = output.status.code.unwrap_or(1);
            let stderr = output.stderr;
            runs.push(ExecRunRecord {
                commit,
                command: command.clone(),
                exit_code,
                stdout: output.stdout,
                stderr: stderr.clone(),
            });
            if exit_code != 0 {
                let _ = restore_exec_prestate(repo, &prestate, current_tree);
                return Err(HistoryError::ExecFailed {
                    commit,
                    exit_code,
                    stderr,
                });
            }
        }
    }

    restore_exec_prestate(repo, &prestate, current_tree)?;
    Ok(ExecReport { runs })
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
    if err.to_string().contains("uncommitted changes") {
        HistoryError::UncommittedChanges
    } else {
        HistoryError::WorktreeApply {
            message: err.to_string(),
        }
    }
}

fn capture_exec_prestate(repo: &Repository) -> Result<ExecPrestate, HistoryError> {
    let head_state = resolve_head(repo.git_dir()).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    let head_commit = resolve_head_commit_oid(repo)?;
    let head_tree = commit_tree_for_apply(repo, head_commit)?;
    let index_bytes = fs::read(repo.index_path()).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    let head = match head_state {
        HeadState::Branch { refname, .. } => SavedHead::Symbolic(refname),
        HeadState::Detached { oid } => SavedHead::Detached(oid),
        HeadState::Invalid => {
            return Err(HistoryError::WorktreeApply {
                message: "HEAD is in an unknown state".into(),
            });
        }
    };
    Ok(ExecPrestate {
        head,
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
            write_symbolic_ref(repo.git_dir(), "HEAD", refname).map_err(|e| {
                HistoryError::WorktreeApply {
                    message: e.to_string(),
                }
            })?;
        }
        SavedHead::Detached(oid) => {
            write_ref(repo.git_dir(), "HEAD", oid).map_err(|e| HistoryError::WorktreeApply {
                message: e.to_string(),
            })?;
        }
    }
    fs::write(repo.index_path(), &pre.index_bytes).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    checkout_hard_to_tree_from_head_tree(repo, &current_tree, &pre.head_tree).map_err(|e| {
        HistoryError::WorktreeApply {
            message: e.to_string(),
        }
    })?;
    Ok(())
}

fn resolve_head_commit_oid(repo: &Repository) -> Result<ObjectId, HistoryError> {
    let head = resolve_head(repo.git_dir()).map_err(|e| HistoryError::WorktreeApply {
        message: e.to_string(),
    })?;
    head.oid()
        .copied()
        .ok_or_else(|| HistoryError::WorktreeApply {
            message: "HEAD has no commit".into(),
        })
}

#[cfg(test)]
mod tests {
    use crate::command_runner::{RecordedResponse, RecordingRunner};
    use crate::history::{
        plan_rewrite, CommitEdit, EmptyPolicy, HistoryEdits, RefScope, RewriteOptions,
    };
    use crate::index::write_tree::write_tree_from_index;
    use crate::objects::parse_commit;
    use crate::objects::{serialize_commit, CommitData, ObjectKind};
    use crate::refs::{resolve_ref, write_ref, write_symbolic_ref};
    use crate::repo::{InitOptions, Repository};

    use super::*;

    fn init_repo() -> (tempfile::TempDir, Repository) {
        let dir = tempfile::tempdir().expect("tempdir");
        let repo = Repository::init(dir.path(), &InitOptions::default()).expect("init");
        (dir, repo)
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
                committer: "T <t@t> 1 +0000".into(),
                preserve_committer: false,
                sign: false,
            },
        )
        .expect("plan")
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
