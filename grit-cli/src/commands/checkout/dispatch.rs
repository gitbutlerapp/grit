//! Dispatch for `grit checkout`.

pub fn dispatch(
    args: crate::args::CheckoutArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv, CommandInteraction};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let CheckoutArgs {
        url,
        slice,
        paths,
        into,
    } = args;
    if slice.is_none() && into.is_none() && paths.is_empty() && !url.contains("://") {
        emit_success(
            commands::switch::run(Some(url), None, false, false, false, false, false)?,
            opts,
            ctx,
        )
    } else {
        emit_success(
            commands::checkout::run(
                &url,
                slice.as_deref(),
                &paths,
                into.as_deref(),
                CommandInteraction::from_mode(opts.mode),
            )?,
            opts,
            ctx,
        )
    }
}
