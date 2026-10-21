//! `grit rm` — remove paths from the working tree and index.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Context, Result};
use grit_lib::pathspec::ResolvedPathspecs;
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions, RemoveOutcome};
use grit_lib::repo::Repository;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// CLI outcome for `grit rm`.
#[derive(Serialize, JsonSchema)]
pub struct RmOutcome {
    /// Paths removed from the index (and work tree unless `--cached`).
    pub removed: Vec<String>,
}

impl HumanRender for RmOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        match self.removed.len() {
            0 => {}
            1 => outln!(w, "Removed {}.", self.removed[0]),
            n => outln!(w, "Removed {n} paths."),
        }
        Ok(())
    }
}

impl MarkdownRender for RmOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            outln!(w, "- **removed**: _(none)_");
            return Ok(());
        }
        outln!(w, "- **removed**:");
        for path in &self.removed {
            outln!(w, "  - `{path}`");
        }
        Ok(())
    }
}

pub fn run(args: &PathspecArgs, cached: bool, force: bool) -> Result<RmOutcome> {
    if args.paths.is_empty() && args.pathspec_from_file.is_none() {
        bail!("you must specify path(s) to remove");
    }
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let outcome = rm(&repo, &resolved, cached, force)?;
    Ok(RmOutcome {
        removed: outcome.removed,
    })
}

fn rm(
    repo: &Repository,
    resolved: &ResolvedPathspecs,
    cached: bool,
    force: bool,
) -> Result<RemoveOutcome> {
    let work_tree = repo.work_tree().context("grit rm needs a working tree")?;

    let mut recursive = false;
    for (sel, spec) in resolved.sources.iter().zip(resolved.specs.iter()) {
        let trailing_dir = sel.ends_with('/') || sel.ends_with('\\');
        if trailing_dir || work_tree.join(spec).is_dir() {
            recursive = true;
        }
    }

    let opts = RemoveOptions {
        pathspecs: resolved.specs.clone(),
        pathspec_sources: resolved.sources.clone(),
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
            anyhow::anyhow!("{msg}")
        }
        other => anyhow::anyhow!("{other}"),
    }
}
