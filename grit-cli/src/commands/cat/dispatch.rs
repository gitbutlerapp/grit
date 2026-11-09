//! Dispatch for `grit cat`.

pub fn dispatch(
    args: crate::args::CatArgs,
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
    use crate::output::OutputMode;

    let CatArgs { spec } = args;
    let sink = match opts.mode {
        OutputMode::Human => commands::cat::CatRunSink::Human,
        OutputMode::Json => commands::cat::CatRunSink::Json,
        OutputMode::Markdown => commands::cat::CatRunSink::Markdown,
    };
    emit_success(commands::cat::run(spec, sink)?, opts, ctx)
}
