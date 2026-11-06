//! Update the checked-out worktree when `receive.denyCurrentBranch=updateInstead`.

use std::io::Write;

use crate::config::environment::RepositoryOptions;
use crate::diff::{
    diff_index_to_tree, diff_index_to_worktree_with_options, diff_trees, DiffIndexToWorktreeOptions,
};
use crate::hooks::{run_hook_opts, HookResult, RunHookOptions};
use crate::index::index_file_mtime;
use crate::objects::ObjectId;
use crate::porcelain::checkout::checkout_between_trees;
use crate::porcelain::worktree_guard::{load_worktree_index, prepare_tree_checkout_with_changes};
use crate::repo::Repository;
use crate::repo::RepositoryError;
use crate::state::{resolve_head_for_repository, HeadState};
use crate::worktree::{admin_dirs_match, list_worktrees, worktree_admin_dir, WorktreeEntry};

use super::receive_pack::{DenyCurrentBranch, ReceivePolicy, RefCommand, RefUpdateRefusal};
use super::Result;

/// Non-bare worktree that has `refname` checked out, if any.
pub(crate) fn worktree_for_checked_out_ref(
    repo: &Repository,
    refname: &str,
) -> Result<Option<WorktreeEntry>> {
    for entry in list_worktrees(repo)? {
        if entry.is_bare {
            continue;
        }
        if matches!(&entry.head, HeadState::Branch { refname: r, .. } if r == refname) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

/// When policy is [`DenyCurrentBranch::UpdateInstead`], update index and worktree for this ref.
///
/// Runs after the `update` hook and before the ref is moved. Returns a refusal reason when the
/// deploy must abort the ref update.
pub(crate) fn deploy_for_update_instead(
    repo: &Repository,
    cmd: &RefCommand,
    policy: &ReceivePolicy,
    sideband_out: &mut Option<&mut dyn Write>,
) -> Result<Option<RefUpdateRefusal>> {
    if policy.deny_current_branch != DenyCurrentBranch::UpdateInstead {
        return Ok(None);
    }
    let Some(new_commit) = cmd.new else {
        return Ok(None);
    };
    let Some(entry) = worktree_for_checked_out_ref(repo, &cmd.refname)? else {
        return Ok(None);
    };
    deploy_push_to_checkout(repo, &entry, new_commit, sideband_out)
}

fn deploy_push_to_checkout(
    repo: &Repository,
    entry: &WorktreeEntry,
    new_commit: ObjectId,
    sideband_out: &mut Option<&mut dyn Write>,
) -> Result<Option<RefUpdateRefusal>> {
    let hex = new_commit.to_hex();
    let here = worktree_admin_dir(repo);
    let wt_repo = if admin_dirs_match(&entry.admin_dir, &here) {
        Repository::discover(Some(entry.path.as_path()), &RepositoryOptions::empty())
            .map_err(super::ServeError::Repository)?
    } else {
        repo.open_linked_worktree(&entry.admin_dir, &entry.path)
            .map_err(super::ServeError::Repository)?
    };
    run_deploy_on_repo(&wt_repo, entry, hex.as_str(), new_commit, sideband_out)
}

fn run_deploy_on_repo(
    wt_repo: &Repository,
    entry: &WorktreeEntry,
    new_hex: &str,
    new_commit: ObjectId,
    sideband_out: &mut Option<&mut dyn Write>,
) -> Result<Option<RefUpdateRefusal>> {
    let hook = run_push_to_checkout_hook(wt_repo, entry, new_hex)?;
    relay_hook_output(sideband_out, &hook);
    match hook {
        HookResult::Failed { .. } => return Ok(Some(RefUpdateRefusal::PushCheckoutHookDeclined)),
        HookResult::Success { .. } => return Ok(None),
        HookResult::NotFound => {}
    }
    push_to_deploy_builtin(wt_repo, new_commit)
}

fn run_push_to_checkout_hook(
    repo: &Repository,
    entry: &WorktreeEntry,
    new_hex: &str,
) -> Result<HookResult> {
    let config = repo
        .config()
        .snapshot()
        .map_err(super::ServeError::Repository)?;
    let git_dir = std::path::absolute(repo.git_dir()).map_err(super::ServeError::Io)?;
    let work_tree = std::path::absolute(&entry.path).map_err(super::ServeError::Io)?;
    let env = [
        ("GIT_DIR", git_dir.to_string_lossy().into_owned()),
        ("GIT_WORK_TREE", work_tree.to_string_lossy().into_owned()),
    ];
    let env_refs: Vec<(&str, &str)> = env.iter().map(|(k, v)| (*k, v.as_str())).collect();
    run_hook_opts(
        Some(repo),
        "push-to-checkout",
        &[new_hex],
        config.as_ref(),
        RunHookOptions {
            env_vars: &env_refs,
            cwd: Some(entry.path.as_path()),
            stdout_to_stderr: true,
            ..RunHookOptions::default()
        },
    )
    .map_err(|e| super::ServeError::Protocol(e.to_string()))
}

fn push_to_deploy_builtin(
    repo: &Repository,
    new_commit: ObjectId,
) -> Result<Option<RefUpdateRefusal>> {
    let mut index = load_worktree_index(repo).map_err(super::ServeError::Repository)?;
    if index.has_unmerged_entries() {
        return Ok(Some(RefUpdateRefusal::PushCheckoutUpToDateFailed));
    }
    let head_tree = resolve_head_for_repository(repo)
        .map_err(super::ServeError::Repository)?
        .tree(repo.odb())
        .map_err(super::ServeError::Repository)?;
    let new_tree = repo
        .odb()
        .peel_to_tree(new_commit)
        .map_err(super::ServeError::Repository)?
        .ok_or_else(|| {
            super::ServeError::Protocol(format!("push deploy: {new_commit} is not a commit"))
        })?;
    let odb = repo.odb();
    let index_path = repo
        .index_path_for_env()
        .map_err(super::ServeError::Repository)?;
    let index_mtime = crate::diff::index_mtime_for_diff(&index, index_file_mtime(&index_path));
    let work_tree = repo.work_tree().ok_or(super::ServeError::Repository(
        RepositoryError::RequiresWorkTree.into(),
    ))?;
    let unstaged = diff_index_to_worktree_with_options(
        odb,
        &mut index,
        work_tree,
        DiffIndexToWorktreeOptions {
            index_mtime,
            ignore_submodule_untracked: true,
            simplify_gitlinks: true,
            refresh_fsmonitor: false,
            repository_git_dir: Some(repo.git_dir().to_path_buf()),
            config: repo.config().snapshot().ok(),
            command_runner: Some(repo.command_runner()),
            ..DiffIndexToWorktreeOptions::default()
        },
    )
    .map_err(super::ServeError::Repository)?;
    if !unstaged.0.is_empty() {
        return Ok(Some(RefUpdateRefusal::PushCheckoutUnstagedChanges));
    }
    let staged = diff_index_to_tree(odb, &index, head_tree.as_ref(), true)
        .map_err(super::ServeError::Repository)?;
    if !staged.is_empty() {
        return Ok(Some(RefUpdateRefusal::PushCheckoutStagedChanges));
    }
    let tree_changes = diff_trees(odb, head_tree.as_ref(), Some(&new_tree), "")
        .map_err(super::ServeError::Repository)?;
    if prepare_tree_checkout_with_changes(repo, &tree_changes).is_err() {
        return Ok(Some(RefUpdateRefusal::PushCheckoutFailed));
    }
    if checkout_between_trees(repo, head_tree.as_ref(), &new_tree).is_err() {
        return Ok(Some(RefUpdateRefusal::PushCheckoutFailed));
    }
    Ok(None)
}

fn relay_hook_output(out: &mut Option<&mut dyn Write>, result: &HookResult) {
    let Some(w) = out.as_mut() else {
        return;
    };
    let Some(output) = result.output() else {
        return;
    };
    for chunk in output
        .stdout
        .chunks(65515)
        .chain(output.stderr.chunks(65515))
    {
        let _ = crate::pkt_line::write_sideband_packet(*w, 2, chunk);
    }
}
