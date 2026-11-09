//! `grit fsck` — check repository integrity.

use anyhow::Result;
use grit_lib::maintenance::repository_fsck::FsckOptions;
use grit_lib::progress::{CancelToken, ProgressSink};

use crate::context;
use crate::terminal_progress::{stderr_progress_enabled, TerminalProgress};

pub use crate::render::fsck::*;

/// CLI flags for `grit fsck`.
#[derive(Debug, Clone, Copy)]
pub struct FsckRunOptions {
    /// Skip full object-store validation; check connectivity from ref roots only.
    pub connectivity_only: bool,
    /// List objects not reachable from ref roots (always on for a full fsck).
    pub dangling: bool,
    /// Skip dangling-object enumeration (matches `git fsck --no-dangling`).
    pub no_dangling: bool,
    /// Treat ref-fsck warnings as errors.
    pub strict: bool,
    /// Include info-level object diagnostics in human output (matches Git `--verbose`).
    pub verbose: bool,
    /// Show stderr progress (human mode only).
    pub show_progress: bool,
}

pub fn run(options: FsckRunOptions) -> Result<FsckOutcome> {
    let repo = context::discover()?;
    let config = repo.config().snapshot()?;
    let mut fsck_opts = if options.connectivity_only {
        FsckOptions::connectivity_and_refs(options.strict)
    } else {
        FsckOptions::full_repository(options.strict)
    };
    if options.no_dangling {
        fsck_opts = fsck_opts.with_check_dangling(false);
    } else if options.connectivity_only {
        fsck_opts = fsck_opts.with_check_dangling(options.dangling);
    }

    let progress_enabled = options.show_progress && stderr_progress_enabled();
    let mut progress = TerminalProgress::new(progress_enabled);
    let report = repo
        .maintenance()
        .fsck(&fsck_opts, &mut progress, &CancelToken::never())?;
    progress.finish();

    Ok(FsckOutcome::from_report(
        &report,
        &config,
        &fsck_opts,
        options.verbose,
    ))
}

/// Exit code helper: non-zero when error-severity issues were found.
pub fn exit_code(outcome: &FsckOutcome) -> i32 {
    if outcome.ok {
        0
    } else {
        1
    }
}

/// Dispatch parsed CLI args for `Fsck`.
pub fn dispatch(
    args: crate::args::FsckArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let FsckArgs {
        connectivity_only,
        dangling,
        no_dangling,
        strict,
        verbose,
    } = args;
    {
        let show_progress = opts.show_progress();
        let outcome = commands::fsck::run(commands::fsck::FsckRunOptions {
            connectivity_only,
            dangling,
            no_dangling,
            strict,
            verbose,
            show_progress,
        })?;
        let status = ExitStatus::from_fsck(&outcome);
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}
