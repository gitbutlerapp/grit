//! `grit rm` — remove paths from the working tree and index.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::porcelain::paths::{RemoveOptions, RemoveOutcome};
use grit_lib::repo::Repository;
use grit_lib::worktree::pathspec::Pathspec;

use crate::context;
use crate::ui::PathDisplayOptions;

pub use crate::render::rm::RmOutcome;

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
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let repo = context::discover()?;
    let path_display = PathDisplayOptions::from_repo(&repo).ok();
    let resolved = args.resolve(&repo)?;
    let outcome = rm(
        &repo,
        resolved,
        cached,
        force,
        dry_run,
        ignore_unmatch,
        &crate::interrupt::process_cancel_token(),
        path_display.as_ref(),
    )?;
    Ok(RmOutcome::new(outcome.removed, dry_run, path_display))
}

#[allow(clippy::too_many_arguments)]
fn rm(
    repo: &Repository,
    resolved: Pathspec,
    cached: bool,
    force: bool,
    dry_run: bool,
    ignore_unmatch: bool,
    cancel: &grit_lib::progress::CancelToken,
    path_display: Option<&PathDisplayOptions>,
) -> Result<RemoveOutcome> {
    let pathspec_sources: Vec<String> = resolved.iter().map(str::to_owned).collect();
    let opts = RemoveOptions {
        pathspecs: resolved.specs().to_vec(),
        pathspec_sources: if pathspec_sources.is_empty() {
            resolved.specs().to_vec()
        } else {
            pathspec_sources
        },
        cached,
        force,
        recursive: false,
        dry_run,
        ignore_unmatch,
    };
    repo.checkout()
        .remove_paths(&opts, cancel)
        .map_err(|err| crate::error_map::map_remove_paths_error(err, cached, path_display))
}

/// Dispatch parsed CLI args for `Rm`.
pub fn dispatch(
    args: crate::args::RmArgs,
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
    let RmArgs {
        pathspecs,
        cached,
        force,
        dry_run,
        ignore_unmatch,
    } = args;
    emit_success(
        commands::rm::run(&pathspecs, cached, force, dry_run, ignore_unmatch)?,
        opts,
        ctx,
    )
}
