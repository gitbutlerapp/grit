//! `grit worktree` — list, add, remove, lock, and unlock linked worktrees.

use anyhow::{bail, Context, Result};
use grit_lib::config::git_path::{
    absolute_path_for_gitfile_stored_path, path_for_disk_compare, path_lexical_for_disk_compare,
    user_display_path,
};
use grit_lib::refs::check_ref_format::validate_branch_short_name;
use grit_lib::repo_handles::LockWorktreeOptions;
use grit_lib::state::HeadState;
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, move_linked_worktree, prune_linked_worktrees,
    remove_linked_worktree, resolve_linked_worktree_add_path, resolve_move_destination,
    AddLinkedWorktreeOptions, MoveWorktreeOptions, PrunableReason, PruneWorktreesOptions,
    WorktreeBranch, WorktreeRepairKind,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ref_name_messages::branch_short_name_error_message;

/// Result of `grit worktree`, tagged by `action`.
#[derive(Serialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorktreeOutcome {
    List {
        worktrees: Vec<WorktreeRow>,
    },
    Add {
        path: String,
        branch: Option<String>,
        head: String,
        locked: bool,
        checked_out: bool,
    },
    Remove {
        path: String,
    },
    Prune {
        dry_run: bool,
        pruned: Vec<PrunedWorktreeRow>,
    },
    Lock {
        path: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        #[schemars(skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Unlock {
        path: String,
    },
    Move {
        from: String,
        to: String,
        dry_run: bool,
    },
    Repair {
        repairs: Vec<WorktreeRepairRow>,
        dry_run: bool,
    },
}

#[derive(Serialize, JsonSchema)]
pub struct WorktreeRepairRow {
    pub path: String,
    pub kind: String,
}

#[derive(Serialize, JsonSchema)]
pub struct WorktreeRow {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    pub lock_reason: Option<String>,
    pub prunable: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub prunable_reason: Option<String>,
    pub is_current: bool,
}

#[derive(Serialize, JsonSchema)]
pub struct PrunedWorktreeRow {
    pub path: String,
    pub reason: String,
}

impl HumanRender for WorktreeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            WorktreeOutcome::List { worktrees } => {
                for wt in worktrees {
                    if wt.bare {
                        outln!(w, "  {}  (bare)", wt.path);
                        continue;
                    }
                    let branch = wt
                        .branch
                        .as_deref()
                        .map(|b| format!(" [{b}]"))
                        .unwrap_or_else(|| {
                            if wt.detached {
                                " (detached HEAD)".to_owned()
                            } else {
                                String::new()
                            }
                        });
                    let current = if wt.is_current { "*" } else { " " };
                    let lock = if wt.locked { " (locked)" } else { "" };
                    let prunable = if wt.prunable { " prunable" } else { "" };
                    outln!(w, "{current} {}{branch}{lock}{prunable}", wt.path);
                }
            }
            WorktreeOutcome::Add { path, branch, .. } => {
                if let Some(b) = branch {
                    outln!(w, "Created worktree at {path} on branch {b}");
                } else {
                    outln!(w, "Created worktree at {path} (detached)");
                }
            }
            WorktreeOutcome::Remove { path } => outln!(w, "Removed worktree {path}"),
            WorktreeOutcome::Prune { dry_run, pruned } => {
                if pruned.is_empty() {
                    outln!(w, "No prunable worktrees");
                } else if *dry_run {
                    for row in pruned {
                        outln!(w, "Would prune worktree {} ({})", row.path, row.reason);
                    }
                } else {
                    for row in pruned {
                        outln!(w, "Pruned worktree {} ({})", row.path, row.reason);
                    }
                }
            }
            WorktreeOutcome::Lock { path, reason } => {
                if let Some(r) = reason {
                    outln!(w, "Locked worktree {path} ({r})");
                } else {
                    outln!(w, "Locked worktree {path}");
                }
            }
            WorktreeOutcome::Unlock { path } => outln!(w, "Unlocked worktree {path}"),
            WorktreeOutcome::Move { from, to, dry_run } => {
                if *dry_run {
                    outln!(w, "Would move worktree {from} -> {to}");
                } else {
                    outln!(w, "Moved worktree {from} -> {to}");
                }
            }
            WorktreeOutcome::Repair { repairs, dry_run } => {
                if repairs.is_empty() {
                    outln!(w, "No worktree links to repair");
                } else if *dry_run {
                    for r in repairs {
                        outln!(w, "Would repair {} ({})", r.path, r.kind);
                    }
                } else {
                    for r in repairs {
                        outln!(w, "Repaired {} ({})", r.path, r.kind);
                    }
                }
            }
        }
        Ok(())
    }
}

impl MarkdownRender for WorktreeOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        markdown::print_worktree_outcome(w, self)?;
        Ok(())
    }
}

pub fn run_list() -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let entries = list_worktrees(&repo).context("could not list worktrees")?;
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
        if let Err(err) = validate_branch_short_name(b) {
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
    let entry = add_linked_worktree(
        &repo,
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
    )
    .map_err(anyhow::Error::new)?;
    let (branch_name, head_hex) = head_summary(&entry.head);
    Ok(WorktreeOutcome::Add {
        path: entry.path.display().to_string(),
        branch: branch_name,
        head: head_hex,
        locked: entry.is_locked,
        checked_out: !no_checkout,
    })
}

pub fn run_remove(path: PathBuf, force: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let canonical = path_for_disk_compare(&path);
    remove_linked_worktree(&repo, &canonical, force).map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Remove {
        path: canonical.display().to_string(),
    })
}

pub fn run_prune(dry_run: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let pruned = prune_linked_worktrees(&repo, &PruneWorktreesOptions { dry_run })
        .map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Prune {
        dry_run,
        pruned: pruned
            .into_iter()
            .map(|e| PrunedWorktreeRow {
                path: e.path.display().to_string(),
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
        path: canonical.display().to_string(),
        reason,
    })
}

pub fn run_move(from: PathBuf, to: PathBuf, force: bool, dry_run: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let from_canon = path_for_disk_compare(&from);
    let to_abs = std::path::absolute(&to).unwrap_or(to);
    let to_lexical = path_lexical_for_disk_compare(&to_abs);
    let dest = resolve_move_destination(&from_canon, &to_lexical).map_err(anyhow::Error::new)?;
    move_linked_worktree(
        &repo,
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
        path: canonical.display().to_string(),
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
