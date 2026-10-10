//! `grit clean` — remove untracked files from the working tree.

use anyhow::{bail, Context, Result};
use grit_lib::pathspec::{pathdiff, resolve_pathspec_in_worktree};
use grit_lib::porcelain::clean::{clean_untracked, CleanOptions};
use grit_lib::progress::NullProgress;
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::context;
use crate::output::{HumanRender, MarkdownRender};

/// Result of `grit clean`.
#[derive(Serialize)]
pub struct CleanOutcome {
    /// Paths removed, or that would be removed when `-f` was not passed.
    pub removed: Vec<String>,
    /// Whether this run only previewed removals (no `-f`).
    #[serde(skip)]
    dry_run: bool,
}

impl HumanRender for CleanOutcome {
    fn render_human(&self) {
        if self.removed.is_empty() {
            if self.dry_run {
                println!("Nothing to clean.");
            } else {
                println!("Removed nothing.");
            }
            return;
        }
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        for path in &self.removed {
            println!("{prefix} {path}");
        }
    }
}

impl MarkdownRender for CleanOutcome {
    fn render_markdown(&self) {
        if self.removed.is_empty() {
            println!("No untracked paths to remove.");
            return;
        }
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        println!("## {heading}\n");
        for path in &self.removed {
            println!("- `{path}`");
        }
    }
}

pub fn run(paths: &[String], force: bool, ignored: bool) -> Result<CleanOutcome> {
    let repo = context::discover()?;
    let pathspecs = resolve_clean_pathspecs(&repo, paths)?;
    let dry_run = !force;
    let lib_outcome = clean_untracked(
        &repo,
        &CleanOptions {
            pathspecs,
            directories: true,
            include_ignored: ignored,
            dry_run,
        },
        &mut NullProgress,
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(CleanOutcome {
        removed: lib_outcome.removed,
        dry_run,
    })
}

fn resolve_clean_pathspecs(repo: &Repository, selectors: &[String]) -> Result<Vec<String>> {
    if selectors.is_empty() {
        return Ok(Vec::new());
    }
    let work_tree = repo
        .work_tree
        .as_ref()
        .context("grit clean needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, work_tree);
    let mut specs = Vec::with_capacity(selectors.len());
    for sel in selectors {
        if sel.is_empty() {
            bail!("invalid path ''");
        }
        let resolved = resolve_pathspec_in_worktree(sel, sel, work_tree, prefix.as_deref())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        specs.push(resolved);
    }
    Ok(specs)
}
