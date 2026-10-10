//! `grit rm` — remove paths from the working tree and index.

use anyhow::{bail, Context, Result};
use grit_lib::pathspec::{pathdiff, resolve_pathspec_in_worktree};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions, RemoveOutcome};
use grit_lib::repo::Repository;
use serde::Serialize;

use crate::context;
use crate::output::HumanRender;

/// CLI outcome for `grit rm`.
#[derive(Serialize)]
pub struct RmOutcome {
    /// Paths removed from the index (and work tree unless `--cached`).
    pub removed: Vec<String>,
}

impl HumanRender for RmOutcome {
    fn render_human(&self) {
        match self.removed.len() {
            0 => {}
            1 => println!("Removed {}.", self.removed[0]),
            n => println!("Removed {n} paths."),
        }
    }
}

impl crate::output::MarkdownRender for RmOutcome {
    fn render_markdown(&self) {
        if self.removed.is_empty() {
            println!("- **removed**: _(none)_");
            return;
        }
        println!("- **removed**:");
        for path in &self.removed {
            println!("  - `{path}`");
        }
    }
}

pub fn run(paths: &[String], cached: bool, force: bool) -> Result<RmOutcome> {
    if paths.is_empty() {
        bail!("you must specify path(s) to remove");
    }
    let repo = context::discover()?;
    let outcome = rm(&repo, paths, cached, force)?;
    Ok(RmOutcome {
        removed: outcome.removed,
    })
}

fn rm(repo: &Repository, selectors: &[String], cached: bool, force: bool) -> Result<RemoveOutcome> {
    let work_tree = repo
        .work_tree
        .clone()
        .context("grit rm needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, &work_tree);

    let mut pathspecs = Vec::with_capacity(selectors.len());
    let mut recursive = false;
    for sel in selectors {
        if sel.is_empty() {
            bail!("invalid path ''");
        }
        let trailing_dir = sel.ends_with('/') || sel.ends_with('\\');
        let resolved = resolve_pathspec_in_worktree(sel, sel, &work_tree, prefix.as_deref())
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if trailing_dir || work_tree.join(&resolved).is_dir() {
            recursive = true;
        }
        pathspecs.push(resolved);
    }

    let opts = RemoveOptions {
        pathspecs,
        pathspec_sources: selectors.to_vec(),
        cached,
        force,
        recursive,
    };
    remove_paths(repo, &opts).map_err(map_paths_error)
}

fn map_paths_error(err: grit_lib::error::Error) -> anyhow::Error {
    use grit_lib::error::Error;
    match err {
        Error::PathsHaveLocalModifications { paths } => {
            let mut msg = String::from("the following files have local changes:\n");
            for path in paths {
                msg.push_str("    ");
                msg.push_str(&path);
                msg.push('\n');
            }
            msg.push_str("(use --cached to keep the file, or -f to force removal)");
            anyhow::anyhow!(msg)
        }
        other => anyhow::anyhow!("{other}"),
    }
}
