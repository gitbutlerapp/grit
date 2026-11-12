//! `grit clean` — remove untracked files from the working tree.

use crate::pathspec_args::PathspecArgs;
use anyhow::{bail, Result};
use grit_lib::porcelain::clean::{CleanOptions, CleanScope};
use grit_lib::progress::NullProgress;

use crate::context;
use crate::ui::PathDisplayOptions;

pub use crate::render::clean::CleanOutcome;

/// JSON scope label for [`CleanScope`].
fn scope_label(scope: CleanScope) -> &'static str {
    match scope {
        CleanScope::Untracked => "untracked",
        CleanScope::UntrackedAndIgnored => "untracked_and_ignored",
        CleanScope::OnlyIgnored => "only_ignored",
        _ => "other",
    }
}

pub fn run(
    args: &PathspecArgs,
    force: bool,
    dry_run_flag: bool,
    ignored: bool,
    only_ignored: bool,
    excludes: &[String],
    files_only: bool,
) -> Result<CleanOutcome> {
    if ignored && only_ignored {
        bail!("--ignored and --only-ignored cannot be used together");
    }
    if force && dry_run_flag {
        bail!("--dry-run cannot be used with -f/--force");
    }
    let scope = if only_ignored {
        CleanScope::OnlyIgnored
    } else if ignored {
        CleanScope::UntrackedAndIgnored
    } else {
        CleanScope::Untracked
    };
    let dry_run = !force;

    let repo = context::discover()?;
    let _interrupt = crate::interrupt::InterruptCancel::install();
    let resolved = args.resolve(&repo)?;
    let pathspecs = resolved.specs().to_vec();
    let cancel = crate::interrupt::process_cancel_token();
    let lib_outcome = repo
        .checkout()
        .clean(
            &CleanOptions::default()
                .with_pathspecs(pathspecs)
                .with_directories(true)
                .with_files_only(files_only)
                .with_scope(scope)
                .with_excludes(excludes.to_vec())
                .with_dry_run(dry_run),
            &mut NullProgress,
            &cancel,
        )
        .map_err(crate::json_error::map_lib_error)?;
    Ok(CleanOutcome::new(
        lib_outcome.removed,
        dry_run,
        scope_label(scope).to_owned(),
        PathDisplayOptions::from_repo(&repo).ok(),
    ))
}

/// Dispatch parsed CLI args for `Clean`.
pub fn dispatch(
    args: crate::args::CleanArgs,
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
    let CleanArgs {
        pathspecs,
        force,
        dry_run,
        ignored,
        only_ignored,
        exclude,
        files_only,
    } = args;
    emit_success(
        commands::clean::run(
            &pathspecs,
            force,
            dry_run,
            ignored,
            only_ignored,
            &exclude,
            files_only,
        )?,
        opts,
        ctx,
    )
}
