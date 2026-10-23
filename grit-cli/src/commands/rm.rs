//! `grit rm` — remove paths from the working tree and index.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::porcelain::paths::{remove_paths, RemoveOptions, RemoveOutcome};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::ResolvedPathspecs;
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
    /// Whether this run only previewed removals.
    pub dry_run: bool,
}

impl HumanRender for RmOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        match self.removed.len() {
            0 => {}
            1 => outln!(w, "{prefix} {}.", self.removed[0]),
            n => outln!(w, "{prefix} {n} paths."),
        }
        Ok(())
    }
}

impl MarkdownRender for RmOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        if self.removed.is_empty() {
            outln!(w, "- **{heading}**: _(none)_");
            return Ok(());
        }
        outln!(w, "- **{heading}**:");
        for path in &self.removed {
            outln!(w, "  - `{path}`");
        }
        Ok(())
    }
}

pub fn run(
    args: &PathspecArgs,
    cached: bool,
    force: bool,
    dry_run: bool,
    ignore_unmatch: bool,
) -> Result<RmOutcome> {
    if args.paths.is_empty() && args.pathspec_from_file.is_none() {
        bail!("you must specify path(s) to remove");
    }
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let outcome = rm(&repo, resolved, cached, force, dry_run, ignore_unmatch)?;
    Ok(RmOutcome {
        removed: outcome.removed,
        dry_run,
    })
}

fn rm(
    repo: &Repository,
    resolved: ResolvedPathspecs,
    cached: bool,
    force: bool,
    dry_run: bool,
    ignore_unmatch: bool,
) -> Result<RemoveOutcome> {
    let opts = RemoveOptions {
        pathspecs: resolved.specs,
        pathspec_sources: resolved.sources,
        cached,
        force,
        recursive: false,
        dry_run,
        ignore_unmatch,
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
