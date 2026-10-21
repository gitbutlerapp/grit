//! `grit mv` — rename or move tracked paths.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::paths::move_path;
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::{pathdiff, resolve_pathspec_in_worktree_with_git_dir};
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// CLI outcome for `grit mv`.
#[derive(Serialize, JsonSchema)]
pub struct MvOutcome {
    /// Source path before the move.
    pub from: String,
    /// Destination path after the move.
    pub to: String,
}

impl HumanRender for MvOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "Renamed {} -> {}.", self.from, self.to);
        Ok(())
    }
}

impl MarkdownRender for MvOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        outln!(w, "- **from**: `{}`", self.from);
        outln!(w, "- **to**: `{}`", self.to);
        Ok(())
    }
}

pub fn run(sources: &[String], force: bool) -> Result<MvOutcome> {
    if sources.len() < 2 {
        bail!("usage: grit mv <source>... <destination>");
    }
    let repo = context::discover()?;
    let (srcs, dst) = sources.split_at(sources.len() - 1);
    let dst = &dst[0];
    let resolved_srcs = resolve_paths(&repo, srcs)?;
    let resolved_dst = resolve_one(&repo, dst)?;

    if resolved_srcs.len() == 1 {
        let outcome = move_path(&repo, &resolved_srcs[0], &resolved_dst, force)
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        return Ok(MvOutcome {
            from: outcome.from,
            to: outcome.to,
        });
    }

    for src in &resolved_srcs {
        let base = std::path::Path::new(src)
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(src.as_str());
        let target = format!("{resolved_dst}/{base}");
        move_path(&repo, src, &target, force).map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    Ok(MvOutcome {
        from: resolved_srcs.join(" "),
        to: resolved_dst,
    })
}

fn resolve_paths(repo: &Repository, selectors: &[String]) -> Result<Vec<String>> {
    selectors.iter().map(|s| resolve_one(repo, s)).collect()
}

fn resolve_one(repo: &Repository, sel: &str) -> Result<String> {
    if sel.is_empty() {
        bail!("invalid path ''");
    }
    let work_tree = repo.work_tree().context("grit mv needs a working tree")?;
    let cwd = std::env::current_dir().context("could not read the current directory")?;
    let prefix = pathdiff(&cwd, work_tree);
    resolve_pathspec_in_worktree_with_git_dir(
        sel,
        sel,
        work_tree,
        prefix.as_deref(),
        Some(repo.git_dir()),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}
