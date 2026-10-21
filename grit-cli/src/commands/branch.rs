//! `grit branch` — list branches, or create / delete one.

use anyhow::{Context, Result};
use std::io::Write;

use grit_lib::error::Error;
use grit_lib::porcelain::branch::{create_branch, delete_branch, list_branches};
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;

use crate::context;
use crate::markdown;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit branch`, tagged by `action` (`list` / `create` / `delete`).
#[derive(Serialize, JsonSchema)]
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
#[derive(Serialize, JsonSchema)]
pub struct BranchEntry {
    pub name: String,
    pub current: bool,
}

impl HumanRender for BranchOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List { branches, .. } => {
                if branches.is_empty() {
                    outln!(w, "No branches yet.");
                    return Ok(());
                }
                for branch in branches {
                    if branch.current {
                        outln!(w, "* {}", branch.name);
                    } else {
                        outln!(w, "  {}", branch.name);
                    }
                }
            }
            BranchOutcome::Create { name } => outln!(w, "Created branch {name}"),
            BranchOutcome::Delete {
                name, short_oid, ..
            } => outln!(w, "Deleted branch {name} (was {short_oid})."),
        }
        Ok(())
    }
}

impl MarkdownRender for BranchOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self {
            BranchOutcome::List { branches, .. } => markdown::print_branch_list(w, branches)?,
            BranchOutcome::Create { name } => outln!(w, "Created branch `{name}`."),
            BranchOutcome::Delete {
                name, short_oid, ..
            } => outln!(w, "Deleted branch `{name}` (was `{short_oid}`)."),
        }
        Ok(())
    }
}

/// List, create, or delete a branch depending on `name` and the delete flags.
pub fn run(name: Option<String>, delete: bool, force_delete: bool) -> Result<BranchOutcome> {
    let repo = context::discover()?;
    let delete = delete || force_delete;
    match name {
        None => list(&repo),
        Some(name) if delete => remove(&repo, &name, force_delete),
        Some(name) => create(&repo, &name),
    }
}

fn list(repo: &Repository) -> Result<BranchOutcome> {
    let listing = list_branches(repo).context("could not list branches")?;
    Ok(BranchOutcome::List {
        current: listing.current,
        branches: listing
            .branches
            .into_iter()
            .map(|branch| BranchEntry {
                name: branch.name,
                current: branch.current,
            })
            .collect(),
    })
}

fn create(repo: &Repository, name: &str) -> Result<BranchOutcome> {
    create_branch(repo, name).context("could not create branch")?;
    Ok(BranchOutcome::Create {
        name: name.to_owned(),
    })
}

fn remove(repo: &Repository, name: &str, force_delete: bool) -> Result<BranchOutcome> {
    let deleted = delete_branch(repo, name, force_delete).map_err(|err| match err {
        Error::BranchNotMerged { .. } => anyhow::anyhow!(
            "{err}.\nIf you are sure you want to delete it, run 'grit branch -D {name}'"
        ),
        other => anyhow::Error::new(other),
    })?;
    let oid = deleted.oid.to_hex();
    let short_oid = oid[..7.min(oid.len())].to_owned();
    Ok(BranchOutcome::Delete {
        name: deleted.name,
        oid,
        short_oid,
    })
}
