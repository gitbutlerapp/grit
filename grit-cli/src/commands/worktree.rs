//! `grit worktree` — list, add, and remove linked worktrees.

use anyhow::{bail, Context, Result};
use grit_lib::check_ref_format::validate_branch_short_name;
use grit_lib::state::HeadState;
use grit_lib::worktree::{
    add_linked_worktree, list_worktrees, remove_linked_worktree, AddLinkedWorktreeOptions,
};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
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
                        .unwrap_or_default();
                    let lock = if wt.locked { " (locked)" } else { "" };
                    outln!(w, "  {}{branch}{lock}", wt.path);
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
        }
        Ok(())
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
    let entry = add_linked_worktree(
        &repo,
        &AddLinkedWorktreeOptions {
            path,
            branch,
            new_branch,
            detach,
            start_point,
        },
    )
    .map_err(anyhow::Error::msg)?;
    let (branch_name, head_hex) = head_summary(&entry.head);
    Ok(WorktreeOutcome::Add {
        path: entry.path.display().to_string(),
        branch: branch_name,
        head: head_hex,
    })
}

pub fn run_remove(path: PathBuf, force: bool) -> Result<WorktreeOutcome> {
    let repo = context::discover()?;
    remove_linked_worktree(&repo, &path, force).map_err(anyhow::Error::msg)?;
    Ok(WorktreeOutcome::Remove {
        path: path.display().to_string(),
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
