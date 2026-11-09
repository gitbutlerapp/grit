//! `grit revs` CLI dispatch and presentation mapping.

use anyhow::Result;
use grit_lib::porcelain::log::LogStdoutFormat;

use crate::args::RevsArgs;
use crate::commands;
use crate::commands::revs::{RevsMode, RevsPresentationOptions};
use crate::dispatch::{emit_success, is_broken_pipe, CommandEnv};
use crate::exit_status::ExitStatus;
use crate::output::OutputMode;

pub(crate) fn revs_presentation_from_mode(mode: OutputMode) -> RevsPresentationOptions {
    RevsPresentationOptions {
        format: match mode {
            OutputMode::Human => LogStdoutFormat::Human,
            OutputMode::Json => LogStdoutFormat::Json,
            OutputMode::Markdown => LogStdoutFormat::Markdown,
        },
    }
}

/// Dispatch parsed CLI args for `Revs`.
pub fn dispatch(args: RevsArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let RevsArgs {
        rev,
        pathspecs,
        count,
        left_right,
        cherry_mark,
    } = args;
    let mode = RevsMode {
        count,
        left_right,
        cherry_mark,
    };
    let presentation = revs_presentation_from_mode(opts.mode);
    if opts.filter.is_none() && commands::revs::listing_can_stream(&rev, &pathspecs, mode)? {
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        match commands::revs::stream_listing(rev, pathspecs, presentation, ctx, &mut lock) {
            Ok(()) => return Ok(ExitStatus::Success),
            Err(e) if is_broken_pipe(&e) => return Ok(ExitStatus::Success),
            Err(e) => return Err(e),
        }
    }
    emit_success(
        commands::revs::run(rev, pathspecs, mode, presentation)?,
        opts,
        ctx,
    )
}
