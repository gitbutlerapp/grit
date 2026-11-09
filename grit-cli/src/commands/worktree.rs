//! `grit worktree` — list, add, remove, lock, and unlock linked worktrees.

use anyhow::{bail, Context, Result};
use grit_lib::config::git_path::{
    absolute_path_for_gitfile_stored_path, path_for_disk_compare, path_lexical_for_disk_compare,
    user_display_path,
};
use grit_lib::refs::check_ref_format::RefName;
use grit_lib::repo_handles::LockWorktreeOptions;
use grit_lib::state::HeadState;
use grit_lib::worktree::{
    resolve_linked_worktree_add_path, resolve_move_destination, AddLinkedWorktreeOptions,
    MoveWorktreeOptions, PrunableReason, PruneWorktreesOptions, WorktreeBranch, WorktreeRepairKind,
};
use std::path::{Path, PathBuf};

use crate::context;
use crate::ref_name_messages::branch_short_name_error_message;

pub use crate::render::worktree::*;

pub fn run_list() -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let entries = repo
        .worktrees()
        .list()
        .context("could not list worktrees")?;
    let worktrees = entries.into_iter().map(map_row).collect();
    Ok(WorktreeOutcome::List { worktrees })
}

#[allow(clippy::too_many_arguments)]
pub fn run_add(
    path: PathBuf,
    start_point: Option<String>,
    branch: Option<String>,
    force_branch: Option<String>,
    detach: bool,
    force: bool,
    lock: bool,
    lock_reason: Option<String>,
    no_checkout: bool,
    track: bool,
) -> Result<WorktreeOutcome> {
    let branch_modes = [branch.is_some(), force_branch.is_some(), detach]
        .into_iter()
        .filter(|b| *b)
        .count();
    if branch_modes > 1 {
        bail!("use only one of `-b`, `-B`, or `--detach`");
    }
    if let Some(b) = branch.as_deref().or(force_branch.as_deref()) {
        if let Err(err) = RefName::branch(b).map(|_| ()) {
            bail!(branch_short_name_error_message(b, &err));
        }
    }
    if lock_reason.is_some() && !lock {
        bail!("`--reason` requires `--lock`");
    }

    let branch_mode = if detach {
        Some(WorktreeBranch::Detach)
    } else if let Some(name) = force_branch {
        Some(WorktreeBranch::CreateOrReset(name))
    } else {
        branch.map(WorktreeBranch::Create)
    };

    let repo = context::discover()?;
    let relative_to = std::env::current_dir().context("could not read working directory")?;
    let path = resolve_linked_worktree_add_path(&path, &relative_to).map_err(anyhow::Error::new)?;
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let entry = repo
        .worktrees()
        .add_with_cancel(
            &AddLinkedWorktreeOptions {
                path,
                branch: branch_mode,
                start_point,
                force,
                lock: lock.then(|| LockWorktreeOptions {
                    reason: lock_reason.clone(),
                }),
                no_checkout,
                track,
            },
            &crate::interrupt::process_cancel_token(),
        )
        .map_err(anyhow::Error::new)?;
    let (branch_name, head_hex) = head_summary(&entry.head);
    Ok(WorktreeOutcome::Add {
        path: user_display_path(&entry.path),
        branch: branch_name,
        head: head_hex,
        locked: entry.is_locked,
        checked_out: !no_checkout,
    })
}

pub fn run_remove(path: PathBuf, force: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let canonical = path_for_disk_compare(&path);
    repo.worktrees()
        .remove(&canonical, force)
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Remove {
        path: user_display_path(&canonical),
    })
}

pub fn run_prune(dry_run: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let pruned = repo
        .worktrees()
        .prune(&PruneWorktreesOptions { dry_run })
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Prune {
        dry_run,
        pruned: pruned
            .into_iter()
            .map(|e| PrunedWorktreeRow {
                path: user_display_path(&e.path),
                reason: e.reason,
            })
            .collect(),
    })
}

pub fn run_lock(path: PathBuf, reason: Option<String>) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let canonical = path_for_disk_compare(&path);
    repo.worktrees()
        .lock(
            &canonical,
            &LockWorktreeOptions {
                reason: reason.clone(),
            },
        )
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Lock {
        path: user_display_path(&canonical),
        reason,
    })
}

pub fn run_move(from: PathBuf, to: PathBuf, force: bool, dry_run: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let from_canon = path_for_disk_compare(&from);
    let to_abs = std::path::absolute(&to).unwrap_or(to);
    let to_lexical = path_lexical_for_disk_compare(&to_abs);
    let dest = resolve_move_destination(&from_canon, &to_lexical).map_err(anyhow::Error::new)?;
    repo.worktrees()
        .move_linked(
            &from_canon,
            &to_lexical,
            &MoveWorktreeOptions { force, dry_run },
        )
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Move {
        from: absolute_path_for_gitfile_stored_path(&from_canon),
        to: absolute_path_for_gitfile_stored_path(&dest),
        dry_run,
    })
}

pub fn run_repair(paths: Vec<PathBuf>, dry_run: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let canon_paths: Vec<PathBuf> = paths.iter().map(|p| path_for_disk_compare(p)).collect();
    let path_refs: Vec<&Path> = canon_paths.iter().map(|p| p.as_path()).collect();
    let repairs = repo
        .worktrees()
        .repair(&path_refs, dry_run)
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Repair {
        repairs: repairs
            .into_iter()
            .map(|r| WorktreeRepairRow {
                path: user_display_path(&r.path),
                kind: repair_kind_label(r.kind).to_owned(),
            })
            .collect(),
        dry_run,
    })
}

fn repair_kind_label(kind: WorktreeRepairKind) -> &'static str {
    match kind {
        WorktreeRepairKind::GitFileAdminDir => "git_file_admin_dir",
        WorktreeRepairKind::GitdirWorktreePath => "gitdir_worktree_path",
        WorktreeRepairKind::GitFilePathStyle => "git_file_path_style",
        WorktreeRepairKind::GitdirPathStyle => "gitdir_path_style",
    }
}

pub fn run_unlock(path: PathBuf) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let canonical = path_for_disk_compare(&path);
    repo.worktrees()
        .unlock(&canonical)
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Unlock {
        path: user_display_path(&canonical),
    })
}

fn map_row(entry: grit_lib::worktree::WorktreeEntry) -> WorktreeRow {
    let (branch, head_hex, detached) = match &entry.head {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => (Some(short_name.clone()), oid.to_hex(), false),
        HeadState::Branch { .. } => (None, String::new(), false),
        HeadState::Detached { oid } => (None, oid.to_hex(), true),
        HeadState::Invalid => (None, String::new(), false),
    };
    WorktreeRow {
        path: user_display_path(&entry.path),
        head: head_hex,
        branch,
        detached,
        bare: entry.is_bare,
        locked: entry.is_locked,
        lock_reason: entry.lock_reason,
        prunable: entry.is_prunable,
        prunable_reason: entry
            .prunable_reason
            .map(PrunableReason::porcelain_message)
            .map(str::to_owned),
        is_current: entry.is_current,
    }
}

fn head_summary(head: &HeadState) -> (Option<String>, String) {
    match head {
        HeadState::Branch {
            short_name,
            oid: Some(oid),
            ..
        } => (Some(short_name.clone()), oid.to_hex()),
        HeadState::Detached { oid } => (None, oid.to_hex()),
        _ => (None, String::new()),
    }
}

#[cfg(test)]
mod user_path_tests {
    use super::*;

    #[test]
    fn add_outcome_path_uses_forward_slashes() {
        let path = PathBuf::from(r"\\?\C:\Users\x\wt");
        let shown = user_display_path(&path);
        assert_eq!(shown, "C:/Users/x/wt");
        assert!(!shown.contains('\\'));
    }
}

/// Dispatch parsed CLI args for `Worktree`.
pub fn dispatch(
    args: crate::args::WorktreeArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let WorktreeArgs { action } = args;
    match action {
        WorktreeAction::List => emit_success(commands::worktree::run_list()?, opts, ctx),
        WorktreeAction::Add {
            path,
            start_point,
            branch,
            force_branch,
            detach,
            force,
            lock,
            reason,
            no_checkout,
            track,
        } => emit_success(
            commands::worktree::run_add(
                path,
                start_point,
                branch,
                force_branch,
                detach,
                force,
                lock,
                reason,
                no_checkout,
                track,
            )?,
            opts,
            ctx,
        ),
        WorktreeAction::Remove { path, force } => {
            emit_success(commands::worktree::run_remove(path, force)?, opts, ctx)
        }
        WorktreeAction::Prune { dry_run } => {
            emit_success(commands::worktree::run_prune(dry_run)?, opts, ctx)
        }
        WorktreeAction::Lock { path, reason } => {
            emit_success(commands::worktree::run_lock(path, reason)?, opts, ctx)
        }
        WorktreeAction::Unlock { path } => {
            emit_success(commands::worktree::run_unlock(path)?, opts, ctx)
        }
        WorktreeAction::Move {
            worktree,
            new_path,
            force,
            dry_run,
        } => emit_success(
            commands::worktree::run_move(worktree, new_path, force, dry_run)?,
            opts,
            ctx,
        ),
        WorktreeAction::Repair { paths, dry_run } => {
            emit_success(commands::worktree::run_repair(paths, dry_run)?, opts, ctx)
        }
    }
}
