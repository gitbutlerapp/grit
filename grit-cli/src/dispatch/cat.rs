//! `grit cat` CLI dispatch and presentation mapping.

use anyhow::Result;
use grit_lib::porcelain::cat::{CatPresentationOptions, CatStdoutFormat};

use crate::args::CatArgs;
use crate::commands;
use crate::dispatch::{emit_success, CommandEnv};
use crate::exit_status::ExitStatus;
use crate::output::OutputMode;

pub(crate) fn cat_presentation_from_mode(mode: OutputMode) -> CatPresentationOptions {
    CatPresentationOptions {
        format: match mode {
            OutputMode::Human => CatStdoutFormat::Human,
            OutputMode::Json => CatStdoutFormat::Json,
            OutputMode::Markdown => CatStdoutFormat::Markdown,
        },
    }
}

/// Dispatch parsed CLI args for `Cat`.
pub fn dispatch(args: CatArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let CatArgs { spec } = args;
    emit_success(
        commands::cat::run(spec, cat_presentation_from_mode(opts.mode))?,
        opts,
        ctx,
    )
}
