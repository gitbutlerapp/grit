//! `grit worktree` — list, add, and remove linked worktrees.

use anyhow::{bail, Context, Result};
use grit_lib::check_ref_format::validate_branch_short_name;
use grit_lib::git_path::path_for_disk_compare;
use grit_lib::state::HeadState;
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, prune_linked_worktrees, remove_linked_worktree,
    resolve_linked_worktree_add_path, AddLinkedWorktreeOptions,
};
use serde::Serialize;
use std::path::PathBuf;

use crate::context;
use crate::output::{HumanRender, MarkdownRender};
use crate::ref_name_messages::branch_short_name_error_message;

/// Result of `grit worktree`, tagged by `action`.
#[derive(Serialize)]
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
}

#[derive(Serialize)]
pub struct WorktreeRow {
    pub path: String,
    pub head: String,
    pub branch: Option<String>,
    pub detached: bool,
    pub bare: bool,
    pub locked: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lock_reason: Option<String>,
    pub prunable: bool,
}

impl HumanRender for WorktreeOutcome {
    fn render_human(&self) {
        match self {
            WorktreeOutcome::List { worktrees } => {
                for wt in worktrees {
                    let branch = wt
                        .branch
                        .as_deref()
                        .map(|b| format!(" [{b}]"))
                        .unwrap_or_default();
                    let lock = if wt.locked { " (locked)" } else { "" };
                    let prunable = if wt.prunable { " prunable" } else { "" };
                    println!("  {}{branch}{lock}{prunable}", wt.path);
                }
            }
            WorktreeOutcome::Add { path, branch, .. } => {
                if let Some(b) = branch {
                    println!("Created worktree at {path} on branch {b}");
                } else {
                    println!("Created worktree at {path} (detached)");
                }
            }
            WorktreeOutcome::Remove { path } => println!("Removed worktree {path}"),
            WorktreeOutcome::Prune { pruned } => {
                if pruned.is_empty() {
                    println!("No prunable worktrees");
                } else {
                    for path in pruned {
                        println!("Pruned worktree {path}");
                    }
                }
            }
        }
    }
}

impl MarkdownRender for WorktreeOutcome {}

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
    let path =
        resolve_linked_worktree_add_path(&path, &relative_to).map_err(|e| anyhow::Error::new(e))?;
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
    .map_err(|e| anyhow::Error::new(e))?;
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
    remove_linked_worktree(&repo, &canonical, force).map_err(|e| anyhow::Error::new(e))?;
    Ok(WorktreeOutcome::Remove {
        path: canonical.display().to_string(),
    })
}

pub fn run_prune() -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    let pruned = prune_linked_worktrees(&repo).map_err(|e| anyhow::Error::new(e))?;
    Ok(WorktreeOutcome::Prune {
        pruned: pruned
            .into_iter()
            .map(|p| p.display().to_string())
            .collect(),
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
        path: entry.path.display().to_string(),
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
