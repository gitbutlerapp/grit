//! Dispatch for `grit revs`.

pub fn dispatch(
    args: crate::args::RevsArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, is_broken_pipe, CommandEnv};
    use crate::exit_status::ExitStatus;

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
    {
        use crate::output::OutputMode;

        let mode = commands::revs::RevsMode {
            count,
            left_right,
            cherry_mark,
        };
        let emit = match opts.mode {
            OutputMode::Human => commands::revs::RevsEmitKind::Human,
            OutputMode::Json => commands::revs::RevsEmitKind::Json,
            OutputMode::Markdown => commands::revs::RevsEmitKind::Markdown,
        };
        if opts.filter.is_none() && commands::revs::listing_can_stream(&rev, &pathspecs, mode)? {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            match crate::render::revs_stream::stream_listing(rev, pathspecs, emit, ctx, &mut lock) {
                Ok(()) => return Ok(ExitStatus::Success),
                Err(e) if is_broken_pipe(&e) => return Ok(ExitStatus::Success),
                Err(e) => return Err(e),
            }
        }
        emit_success(commands::revs::run(rev, pathspecs, mode, emit)?, opts, ctx)
    }
}
