//! `grit mv` — rename or move tracked paths.

use anyhow::{bail, Context, Result};
use grit_lib::porcelain::paths::{MoveOptions, PathMove};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::pathdiff;

use crate::context;
use crate::ui::PathDisplayOptions;

pub use crate::render::mv::{MvOutcome, PathMoveJson};

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
            &MoveOptions::new(resolved_srcs, resolved_dst)
                .with_force(force)
                .with_dry_run(dry_run),
            &cancel,
        )
        .map_err(crate::json_error::map_lib_error)?;

    Ok(MvOutcome::new(
        lib_outcome
            .moves
            .into_iter()
            .map(|PathMove { from, to }| PathMoveJson { from, to })
            .collect(),
        dry_run,
        PathDisplayOptions::from_repo(&repo).ok(),
    ))
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
