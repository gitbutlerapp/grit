//! `grit mv` — rename or move tracked paths.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::paths::{move_paths, MoveOptions, PathMove};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::pathdiff;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};

/// One path rename in `grit mv` JSON output.
#[derive(Serialize, JsonSchema, Clone)]
pub struct PathMoveJson {
    /// Source path before the move.
    pub from: String,
    /// Destination path after the move.
    pub to: String,
}

/// CLI outcome for `grit mv`.
#[derive(Serialize, JsonSchema)]
pub struct MvOutcome {
    /// Individual renames performed or previewed.
    pub moves: Vec<PathMoveJson>,
    /// Whether this run only previewed moves.
    pub dry_run: bool,
}

impl HumanRender for MvOutcome {
    fn render_human(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        let prefix = if self.dry_run {
            "Would rename"
        } else {
            "Renamed"
        };
        if self.moves.is_empty() {
            return Ok(());
        }
        if self.moves.len() == 1 {
            let mv = &self.moves[0];
            outln!(w, "{prefix} {} -> {}.", mv.from, mv.to);
            return Ok(());
        }
        for mv in &self.moves {
            outln!(w, "{prefix} {} -> {}.", mv.from, mv.to);
        }
        Ok(())
    }
}

impl MarkdownRender for MvOutcome {
    fn render_markdown(&self, w: &mut dyn Write, _ctx: &RenderContext) -> Result<()> {
        if self.moves.is_empty() {
            outln!(w, "- **moves**: _(none)_");
            return Ok(());
        }
        outln!(w, "- **moves**:");
        for mv in &self.moves {
            outln!(w, "  - `{}` → `{}`", mv.from, mv.to);
        }
        Ok(())
    }
}

pub fn run(sources: &[String], force: bool, dry_run: bool) -> Result<MvOutcome> {
    if sources.len() < 2 {
        bail!("usage: grit mv <source>... <destination>");
    }
    let repo = context::discover()?;
    let (srcs, dst) = sources.split_at(sources.len() - 1);
    let dst = &dst[0];
    let resolved_srcs = resolve_paths(&repo, srcs)?;
    let resolved_dst = resolve_one(&repo, dst)?;

    let lib_outcome = move_paths(
        &repo,
        &MoveOptions {
            sources: resolved_srcs,
            destination: resolved_dst,
            force,
            dry_run,
        },
    )
    .map_err(|e| anyhow::anyhow!("{e}"))?;

    Ok(MvOutcome {
        moves: lib_outcome
            .moves
            .into_iter()
            .map(|PathMove { from, to }| PathMoveJson { from, to })
            .collect(),
        dry_run,
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
    repo.resolve_worktree_pathspec(sel, prefix.as_deref())
        .map_err(|e| anyhow::anyhow!("{e}"))
}
