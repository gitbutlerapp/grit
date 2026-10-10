//! `grit switch` — move to another branch, updating the working tree.
//!
//! `grit` keeps this safe and simple: it refuses to switch when you have
//! uncommitted (staged or unstaged) changes, and won't clobber an untracked
//! file that the destination branch wants to create. Untracked files that don't
//! collide come along for the ride.

use anyhow::{bail, Context, Result};
use grit_lib::check_ref_format::validate_branch_short_name;

use crate::ref_name_messages::branch_short_name_error_message;
use grit_lib::porcelain::checkout::checkout_tree_changes;
use grit_lib::porcelain::worktree_guard::{ensure_no_untracked_overwrite, prepare_tree_switch};
use grit_lib::refs;
use grit_lib::state::resolve_head;
use serde::Serialize;

use crate::context;
use crate::output::{HumanRender, MarkdownRender};

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

impl MarkdownRender for SwitchOutcome {}

pub fn run(name: &str, create: bool) -> Result<SwitchOutcome> {
    let repo = context::discover()?;

    let head_oid = resolve_head(&repo.git_dir)
        .context("could not resolve HEAD")?
        .oid()
        .copied();
    let branch_ref = format!("refs/heads/{name}");

    if create {
        if let Err(err) = validate_branch_short_name(name) {
            bail!(branch_short_name_error_message(name, &err));
        }
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

    let plan = prepare_tree_switch(&repo, head_tree.as_ref(), &target_tree)
        .map_err(anyhow::Error::new)
        .context("could not prepare branch switch")?;
    ensure_no_untracked_overwrite(&plan.untracked, &plan.tree_changes)
        .map_err(anyhow::Error::new)?;
    checkout_tree_changes(&repo, &plan.tree_changes)
        .context("could not update the working tree")?;
    refs::write_symbolic_ref(&repo.git_dir, "HEAD", &branch_ref).context("could not move HEAD")?;

    Ok(SwitchOutcome {
        branch: name.to_owned(),
        created: create,
    })
}
