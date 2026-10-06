//! `grit branch` — list branches, or create / delete one.

use anyhow::{bail, Context, Result};
use grit_lib::branch_tracking::upstream_tracking_full_ref;
use grit_lib::merge_base::is_ancestor;
use grit_lib::refs;
use grit_lib::repo::Repository;
use grit_lib::state::{resolve_head, HeadState};
use serde::Serialize;

use crate::context;
use crate::output::HumanRender;

/// Result of `grit branch`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BranchOutcome {
    List {
        /// The current branch, or `null` when detached / unborn.
        current: Option<String>,
        branches: Vec<BranchEntry>,
    },
    Create {
        name: String,
    },
    Delete {
        name: String,
        /// Full object id of the branch tip before deletion.
        oid: String,
        /// Abbreviated tip id (seven hex digits when possible).
        short_oid: String,
    },
}

/// One branch in a `list` outcome.
#[derive(Serialize)]
pub struct BranchEntry {
    pub name: String,
    pub current: bool,
}

impl HumanRender for BranchOutcome {
    fn render_human(&self) {
        match self {
            BranchOutcome::List { branches, .. } => {
                if branches.is_empty() {
                    println!("No branches yet.");
                    return;
                }
                for branch in branches {
                    if branch.current {
                        println!("* {}", branch.name);
                    } else {
                        println!("  {}", branch.name);
                    }
                }
            }
            BranchOutcome::Create { name } => println!("Created branch {name}"),
            BranchOutcome::Delete {
                name, short_oid, ..
            } => println!("Deleted branch {name} (was {short_oid})."),
        }
    }
}

/// List, create, or delete a branch depending on `name` and the delete flags.
pub fn run(name: Option<String>, delete: bool, force_delete: bool) -> Result<BranchOutcome> {
    let repo = context::discover()?;
    let delete = delete || force_delete;
    match name {
        None => list(&repo),
        Some(name) if delete => delete_branch(&repo, &name, force_delete),
        Some(name) => create(&repo, &name),
    }
}

fn list(repo: &Repository) -> Result<BranchOutcome> {
    let current = match resolve_head(&repo.git_dir)? {
        HeadState::Branch { short_name, .. } => Some(short_name),
        _ => None,
    };

    let mut names: Vec<String> = refs::list_refs(&repo.git_dir, "refs/heads/")
        .context("could not list branches")?
        .into_iter()
        .map(|(refname, _)| {
            refname
                .strip_prefix("refs/heads/")
                .unwrap_or(&refname)
                .to_owned()
        })
        .collect();
    names.sort();

    let branches = names
        .into_iter()
        .map(|name| {
            let current = current.as_deref() == Some(name.as_str());
            BranchEntry { name, current }
        })
        .collect();

    Ok(BranchOutcome::List { current, branches })
}

fn create(repo: &Repository, name: &str) -> Result<BranchOutcome> {
    let branch_ref = format!("refs/heads/{name}");
    if refs::resolve_ref(&repo.git_dir, &branch_ref).is_ok() {
        bail!("branch '{name}' already exists");
    }

    let base = match resolve_head(&repo.git_dir)? {
        HeadState::Branch { oid: Some(oid), .. } | HeadState::Detached { oid } => oid,
        HeadState::Branch { .. } => bail!("no commits yet to create a branch from"),
        HeadState::Invalid => bail!("HEAD is in an unknown state"),
    };

    refs::write_ref(&repo.git_dir, &branch_ref, &base).context("could not create branch")?;
    Ok(BranchOutcome::Create {
        name: name.to_owned(),
    })
}

fn branch_delete_base_oid(
    repo: &Repository,
    branch_name: &str,
    head: &HeadState,
) -> Option<grit_lib::objects::ObjectId> {
    if let Some(upstream_ref) = upstream_tracking_full_ref(repo, branch_name) {
        if let Ok(oid) = refs::resolve_ref(&repo.git_dir, &upstream_ref) {
            return Some(oid);
        }
    }
    head.oid().copied()
}

fn delete_branch(repo: &Repository, name: &str, force_delete: bool) -> Result<BranchOutcome> {
    if let HeadState::Branch { short_name, .. } = resolve_head(&repo.git_dir)? {
        if short_name == name {
            bail!("cannot delete '{name}' — it is the current branch");
        }
    }

    let branch_ref = format!("refs/heads/{name}");
    let branch_oid = refs::resolve_ref(&repo.git_dir, &branch_ref)
        .map_err(|_| anyhow::anyhow!("no branch named '{name}'"))?;

    let head = resolve_head(&repo.git_dir)?;
    if !force_delete {
        let merged = branch_delete_base_oid(repo, name, &head)
            .map(|base_oid| is_ancestor(repo, branch_oid, base_oid).unwrap_or(false))
            .unwrap_or(false);
        if !merged {
            bail!(
                "the branch '{name}' is not fully merged.\nIf you are sure you want to delete it, run 'grit branch -D {name}'"
            );
        }
    }

    refs::delete_ref(&repo.git_dir, &branch_ref).context("could not delete branch")?;

    let hex = branch_oid.to_hex();
    let short_len = 7.min(hex.len());
    let short_oid = hex[..short_len].to_owned();

    Ok(BranchOutcome::Delete {
        name: name.to_owned(),
        oid: hex,
        short_oid,
    })
}
