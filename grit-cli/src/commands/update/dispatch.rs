//! Dispatch for `grit update`.

pub fn dispatch(
    _args: crate::args::UpdateArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;

    use crate::dispatch::CommandInteraction;

    emit_success(
        commands::update::run(CommandInteraction::from_mode(opts.mode))?,
        opts,
        ctx,
    )
}
