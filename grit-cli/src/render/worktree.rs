//! `grit worktree` outcomes and rendering.

use crate::markdown;
use crate::output::outln;
use crate::render::{HumanRender, MarkdownRender, RenderContext};
use anyhow::Result;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

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
