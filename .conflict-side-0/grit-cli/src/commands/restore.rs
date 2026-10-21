//! `grit restore` — restore working tree and/or index paths.

use anyhow::{Context, Result};
use grit_lib::porcelain::restore::{restore_paths, RestoreOptions, RestoreOutcome, RestoreSource};
use grit_lib::revwalk::rev_parse::resolve_revision;
use grit_lib::worktree::pathspec::{pathdiff, resolve_pathspec_in_worktree_with_git_dir};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

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
    paths: Vec<String>,
    staged: bool,
    worktree: bool,
    source: Option<String>,
) -> Result<RestoreCliOutcome> {
    let repo = context::discover()?;
    let work_tree = repo
        .work_tree()
        .context("grit restore needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, work_tree);

    let mut pathspecs = Vec::with_capacity(paths.len());
    for sel in &paths {
        let resolved = resolve_pathspec_in_worktree_with_git_dir(
            sel,
            sel,
            work_tree,
            prefix.as_deref(),
            Some(repo.git_dir()),
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        pathspecs.push(resolved);
    }

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
        pathspec_sources: paths,
        source: restore_source,
        staged,
        worktree,
    };

    let outcome = restore_paths(&repo, &opts).map_err(|e| anyhow::anyhow!("{e}"))?;
    Ok(outcome.into())
}
