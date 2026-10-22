//! `grit clean` — remove untracked files from the working tree.

use crate::pathspec_args::PathspecArgs;
use anyhow::Result;
use grit_lib::porcelain::clean::{clean_untracked, CleanOptions};
use grit_lib::progress::NullProgress;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// Result of `grit clean`.
#[derive(Serialize, JsonSchema)]
pub struct CleanOutcome {
    /// Paths removed, or that would be removed when `-f` was not passed.
    pub removed: Vec<String>,
    /// Whether this run only previewed removals (no `-f`).
    pub dry_run: bool,
}

impl HumanRender for CleanOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            if self.dry_run {
                outln!(w, "Nothing to clean.");
            } else {
                outln!(w, "Removed nothing.");
            }
            return Ok(());
        }
        let prefix = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        for path in &self.removed {
            outln!(w, "{prefix} {path}");
        }
        Ok(())
    }
}

impl MarkdownRender for CleanOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.removed.is_empty() {
            outln!(w, "No untracked paths to remove.");
            return Ok(());
        }
        let heading = if self.dry_run {
            "Would remove"
        } else {
            "Removed"
        };
        outln!(w, "## {heading}\n");
        for path in &self.removed {
            outln!(w, "- `{path}`");
        }
        Ok(())
    }
}

pub fn run(args: &PathspecArgs, force: bool, ignored: bool) -> Result<CleanOutcome> {
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.into_specs();
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
