//! `grit worktree` — list, add, remove, lock, and unlock linked worktrees.

use anyhow::{bail, Context, Result};
use grit_lib::config::git_path::{path_for_disk_compare, user_display_path};
use grit_lib::refs::check_ref_format::validate_branch_short_name;
use grit_lib::repo_handles::LockWorktreeOptions;
use grit_lib::state::HeadState;
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, prune_linked_worktrees, remove_linked_worktree,
    resolve_linked_worktree_add_path, AddLinkedWorktreeOptions,
};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

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
    },
    Remove {
        path: String,
    },
    Prune {
        pruned: Vec<String>,
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
}

#[derive(Serialize, JsonSchema)]
pub struct WorktreeRow {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    #[schemars(skip_serializing_if = "Option::is_none")]
    pub lock_reason: Option<String>,
    pub prunable: bool,
}

impl HumanRender for WorktreeOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            WorktreeOutcome::List { worktrees } => {
                for wt in worktrees {
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
                    let lock = if wt.locked { " (locked)" } else { "" };
                    let prunable = if wt.prunable { " prunable" } else { "" };
                    outln!(w, "  {}{branch}{lock}{prunable}", wt.path);
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
            WorktreeOutcome::Prune { pruned } => {
                if pruned.is_empty() {
                    outln!(w, "No prunable worktrees");
                } else {
                    for path in pruned {
                        outln!(w, "Pruned worktree {path}");
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

pub fn run_add(
    path: PathBuf,
    start_point: Option<String>,
    branch: Option<String>,
    new_branch: bool,
    detach: bool,
    force: bool,
) -> Result<WorktreeOutcome> {
    if new_branch {
        if let Some(ref b) = branch {
            if let Err(err) = validate_branch_short_name(b) {
                bail!(branch_short_name_error_message(b, &err));
            }
        } else {
            bail!("`-b` requires a branch name");
        }
    }
    if detach && branch.is_some() {
        bail!("use either `--detach` or `-b`, not both");
    }
    let repo = context::discover()?;
    let relative_to = std::env::current_dir().context("could not read working directory")?;
    let path = resolve_linked_worktree_add_path(&path, &relative_to).map_err(anyhow::Error::new)?;
    let entry = add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path,
            branch,
            new_branch,
            detach,
            start_point,
            force,
        },
    )
    .map_err(anyhow::Error::new)?;
    let (branch_name, head_hex) = head_summary(&entry.head);
    Ok(WorktreeOutcome::Add {
        path: entry.path.display().to_string(),
        branch: branch_name,
        head: head_hex,
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

pub fn run_prune() -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let pruned = prune_linked_worktrees(&repo).map_err(anyhow::Error::new)?;
    Ok(WorktreeOutcome::Prune {
        pruned: pruned
            .into_iter()
            .map(|p| p.display().to_string())
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
