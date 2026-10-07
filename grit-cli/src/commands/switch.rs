//! `grit switch` — move to another branch, updating the working tree.
//!
//! `grit` keeps this safe and simple: it refuses to switch when you have
//! uncommitted (staged or unstaged) changes, and won't clobber an untracked
//! file that the destination branch wants to create. Untracked files that don't
//! collide come along for the ride.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use grit_lib::diff::{diff_trees, DiffStatus};
use grit_lib::porcelain::status::{status, StatusModel, StatusOptions};
use grit_lib::progress::NullProgress;
use grit_lib::refs;
use grit_lib::state::resolve_head;
use serde::Serialize;

use crate::context;
use crate::output::HumanRender;

/// Result of `grit switch`.
#[derive(Serialize)]
pub struct SwitchOutcome {
    pub branch: String,
    /// Whether the branch was created (`-c`) as part of the switch.
    pub created: bool,
}

impl HumanRender for SwitchOutcome {
    fn render_human(&self) {
        if self.created {
            println!("Created and switched to branch {}", self.branch);
        } else {
            println!("Switched to branch {}", self.branch);
        }
    }
}

pub fn run(name: &str, create: bool) -> Result<SwitchOutcome> {
    let repo = context::discover()?;

    let model = status(&repo, &StatusOptions::default(), &mut NullProgress)
        .context("could not compute status")?;
    if !model.staged.is_empty() || !model.unstaged.is_empty() {
        bail!("you have uncommitted changes — commit them before switching");
    }

    let head_oid = resolve_head(&repo.git_dir)
        .context("could not resolve HEAD")?
        .oid()
        .copied();
    let branch_ref = format!("refs/heads/{name}");

    if create {
        if refs::resolve_ref(&repo.git_dir, &branch_ref).is_ok() {
            bail!("branch '{name}' already exists");
        }
        let Some(base) = head_oid else {
            bail!("no commits yet to create a branch from");
        };
        refs::write_ref(&repo.git_dir, &branch_ref, &base).context("could not create branch")?;
    }

    let target_oid = refs::resolve_ref(&repo.git_dir, &branch_ref)
        .with_context(|| format!("no branch named '{name}'"))?;

    let target_tree = context::commit_tree(&repo, &target_oid)?;
    let head_tree = match head_oid {
        Some(oid) => Some(context::commit_tree(&repo, &oid)?),
        None => None,
    };

    let changes = diff_trees(&repo.odb, head_tree.as_ref(), Some(&target_tree), "")?;
    guard_untracked_changes(&model, &changes)?;
    grit_lib::porcelain::checkout::checkout_tree_changes(&repo, &changes)
        .context("could not update the working tree")?;
    refs::write_symbolic_ref(&repo.git_dir, "HEAD", &branch_ref).context("could not move HEAD")?;

    Ok(SwitchOutcome {
        branch: name.to_owned(),
        created: create,
    })
}

/// Refuse the switch if it would overwrite an untracked working-tree file with a
/// path the destination branch newly introduces.
fn guard_untracked_changes(
    model: &StatusModel,
    changes: &[grit_lib::diff::DiffEntry],
) -> Result<()> {
    if model.untracked.is_empty() {
        return Ok(());
    }
    let untracked: HashSet<&str> = model.untracked.iter().map(String::as_str).collect();

    for change in changes {
        if change.status != DiffStatus::Added {
            continue;
        }
        if let Some(path) = &change.new_path {
            if untracked.contains(path.as_str()) {
                bail!("untracked file '{path}' would be overwritten — move or remove it first");
            }
        }
    }
    Ok(())
}
