//! `grit restore` — restore working tree and/or index paths.

use anyhow::{Context, Result};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreOutcome, RestoreSource};
use grit_lib::revwalk::rev_parse::resolve_revision;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::pathspec_args::PathspecArgs;

/// CLI result (mirrors [`RestoreOutcome`]).
#[derive(Serialize, JsonSchema, Clone)]
pub struct RestoreCliOutcome {
    pub restored: Vec<String>,
    pub removed: Vec<String>,
}

impl From<RestoreOutcome> for RestoreCliOutcome {
    fn from(value: RestoreOutcome) -> Self {
        Self {
            restored: value.restored,
            removed: value.removed,
        }
    }
}

impl HumanRender for RestoreCliOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        for path in &self.restored {
            outln!(w, "Restored {path}");
        }
        for path in &self.removed {
            outln!(w, "Removed {path}");
        }
        Ok(())
    }
}

impl MarkdownRender for RestoreCliOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "## Restore");
        if self.restored.is_empty() && self.removed.is_empty() {
            outln!(w, "\nNo paths changed.");
            return Ok(());
        }
        if !self.restored.is_empty() {
            outln!(w, "\n### Restored");
            for path in &self.restored {
                outln!(w, "- `{path}`");
            }
        }
        if !self.removed.is_empty() {
            outln!(w, "\n### Removed");
            for path in &self.removed {
                outln!(w, "- `{path}`");
            }
        }
        Ok(())
    }
}

/// Run `grit restore` with the given flags and path arguments.
pub fn run(
    args: &PathspecArgs,
    staged: bool,
    worktree: bool,
    source: Option<String>,
) -> Result<RestoreCliOutcome> {
    args.require_non_empty()?;
    let repo = context::discover()?;
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.specs.clone();

    let restore_source = match source {
        None => RestoreSource::Index,
        Some(rev) => {
            let oid = resolve_revision(&repo, &rev)
                .with_context(|| format!("could not resolve '{rev}'"))?;
            let obj = repo.odb().read(&oid).context("reading source object")?;
            use grit_lib::objects::{parse_commit, ObjectKind};
            let tree = match obj.kind {
                ObjectKind::Commit => parse_commit(&obj.data)?.tree,
                ObjectKind::Tree => oid,
                ObjectKind::Blob | ObjectKind::Tag => {
                    anyhow::bail!("source must be a commit or tree");
                }
            };
            RestoreSource::Tree(tree)
        }
    };

    let opts = RestoreOptions {
        pathspecs: pathspecs.clone(),
        pathspec_sources: resolved.sources.clone(),
        source: restore_source,
        staged,
        worktree,
    };

    let outcome = restore_paths(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(outcome.into())
}
