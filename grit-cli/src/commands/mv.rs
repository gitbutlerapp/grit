//! `grit mv` — rename or move tracked paths.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::paths::{MoveOptions, PathMove};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::pathdiff;
use schemars::JsonSchema;
use serde::Serialize;
use std::io::Write;

use crate::context;
use crate::markdown::format_inline_code;
use crate::output::{outln, HumanRender, MarkdownRender, RenderContext};
use crate::ui::PathDisplayOptions;

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
    #[serde(skip)]
    #[schemars(skip)]
    path_display: Option<PathDisplayOptions>,
}

impl MvOutcome {
    fn format_path(&self, path: &str) -> String {
        self.path_display
            .as_ref()
            .map(|opts| opts.format_path_for_human(path))
            .unwrap_or_else(|| path.to_owned())
    }
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
            outln!(
                w,
                "{prefix} {} -> {}.",
                self.format_path(&mv.from),
                self.format_path(&mv.to)
            );
            return Ok(());
        }
        for mv in &self.moves {
            outln!(
                w,
                "{prefix} {} -> {}.",
                self.format_path(&mv.from),
                self.format_path(&mv.to)
            );
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
            let from = format_inline_code(&self.format_path(&mv.from));
            let to = format_inline_code(&self.format_path(&mv.to));
            outln!(w, "  - {from} → {to}");
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

    let _interrupt = crate::interrupt::InterruptCancel::install();
    let cancel = crate::interrupt::process_cancel_token();
    let lib_outcome = repo
        .checkout()
        .move_paths(
            &MoveOptions {
                sources: resolved_srcs,
                destination: resolved_dst,
                force,
                dry_run,
            },
            &cancel,
        )
        .map_err(crate::json_error::map_lib_error)?;

    Ok(MvOutcome {
        moves: lib_outcome
            .moves
            .into_iter()
            .map(|PathMove { from, to }| PathMoveJson { from, to })
            .collect(),
        dry_run,
        path_display: PathDisplayOptions::from_repo(&repo).ok(),
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
        .map_err(anyhow::Error::new)
}

/// Dispatch parsed CLI args for `Mv`.
pub fn dispatch(
    args: crate::args::MvArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let MvArgs {
        paths,
        force,
        dry_run,
    } = args;
    emit_success(commands::mv::run(&paths, force, dry_run)?, opts, ctx)
}
