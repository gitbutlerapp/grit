//! Shared dispatch helpers.

use anyhow::{bail, Result};

use crate::args::{Cli, Command, HistoryAction, StashAction};
use crate::commands;
use crate::exit_status::ExitStatus;
use crate::json_error;
use crate::output::{self, emit, OutputMode, OutputOptions, RenderContext};
use crate::quiet_args;

/// Invocation context shared by every command `dispatch`.
pub struct CommandEnv<'a> {
    pub opts: &'a OutputOptions,
    pub ctx: &'a RenderContext,
    pub process_env: &'a grit_lib::config::environment::Environment,
}

/// Commands that write objects and so may start automatic maintenance when they succeed.
pub(crate) fn writes_objects(command: &Command) -> bool {
    match command {
        Command::Commit { args, .. } => !args.dry_run,
        Command::Merge { args, .. } => !args.check && !args.dry_run,
        Command::Pick { args, .. } => !args.check && !args.dry_run,
        Command::Revert { args, .. } => !args.check && !args.dry_run,
        Command::Fetch { args } => !args.dry_run,
        Command::Pull { args, .. } => !args.fetch.dry_run && !args.check,
        Command::Rebase { args, .. } => !args.shared.dry_run,
        Command::History { args } => !history_action_shared(&args.action).dry_run,
        Command::Stash { args, .. } => {
            !args.dry_run
                && matches!(
                    args.action,
                    None | Some(StashAction::Push { dry_run: false, .. })
                )
        }
        _ => false,
    }
}

/// Commands that may run credential helpers or remote transports.
pub(crate) fn talks_to_remotes(command: &Command) -> bool {
    matches!(
        command,
        Command::Push { .. }
            | Command::Fetch { .. }
            | Command::Pull { .. }
            | Command::Clone { .. }
            | Command::Remote { .. }
            | Command::Auth { .. }
            | Command::Checkout { .. }
            | Command::Checkin { .. }
            | Command::Integrate { .. }
            | Command::Sync { .. }
            | Command::Share { .. }
            | Command::Slice { .. }
            | Command::Serve { .. }
            | Command::ReceivePack { .. }
            | Command::UploadPack { .. }
    )
}

fn history_action_shared(action: &HistoryAction) -> &commands::history::HistoryRewriteArgs {
    match action {
        HistoryAction::Drop { shared, .. }
        | HistoryAction::Move { shared, .. }
        | HistoryAction::Reword { shared, .. }
        | HistoryAction::Fixup { shared, .. }
        | HistoryAction::Absorb { shared, .. }
        | HistoryAction::Rebase { shared, .. }
        | HistoryAction::Split { shared, .. }
        | HistoryAction::Undo { shared }
        | HistoryAction::Squash { shared, .. } => shared,
    }
}

pub fn validate_cli(cli: &Cli) -> Result<(), clap::Error> {
    if let Some(Command::Blame { args, .. }) = &cli.command {
        if args.copy_detection > 3 {
            return Err(clap::Error::raw(
                clap::error::ErrorKind::InvalidValue,
                "blame copy detection: at most three -C flags are supported",
            ));
        }
    }
    Ok(())
}

/// Plumbing commands whose stdout is not a command outcome object.
pub(crate) fn reject_non_human_plumbing(cli: &Cli, opts: &OutputOptions) -> Result<()> {
    if opts.mode == OutputMode::Human {
        return Ok(());
    }
    let Some(cmd) = &cli.command else {
        return Ok(());
    };
    let name = match cmd {
        Command::Completions { .. } => "completions",
        Command::Manager { .. } => "manager",
        Command::UploadPack { .. } => "upload-pack",
        Command::ReceivePack { .. } => "receive-pack",
        _ => return Ok(()),
    };
    bail!("`grit {name}` does not support --json or --markdown");
}

pub(crate) fn is_broken_pipe(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|e| e.kind() == std::io::ErrorKind::BrokenPipe)
    })
}

pub(crate) fn emit_success<T: output::HumanRender + output::MarkdownRender + serde::Serialize>(
    value: T,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus> {
    emit(value, opts, ctx)?;
    Ok(ExitStatus::Success)
}

/// Like [`emit_success`], but renders nothing in human mode under `--quiet`.
pub(crate) fn emit_success_quiet<
    T: output::HumanRender + output::MarkdownRender + serde::Serialize,
>(
    value: T,
    quiet: &quiet_args::QuietArgs,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus> {
    if quiet.shows_outcome(opts.mode) {
        emit(value, opts, ctx)?;
    }
    Ok(ExitStatus::Success)
}

pub(crate) fn finish_hunk_error(err: anyhow::Error, opts: &OutputOptions, code: i32) -> ExitStatus {
    let err = json_error::ensure_structured(err);
    if opts.mode == OutputMode::Human {
        crate::render::error::emit_fatal_line(&err);
    } else {
        output::emit_error(&err, opts);
    }
    ExitStatus::from_i32(code)
}

pub(crate) fn emit_rebase_outcome(
    outcome: commands::history::RebaseRunOutcome,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus> {
    let status = ExitStatus::from_rebase_run(&outcome);
    emit(outcome, opts, ctx)?;
    Ok(status)
}
