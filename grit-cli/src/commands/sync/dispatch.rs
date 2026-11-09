//! Dispatch for `grit sync`.

pub fn dispatch(
    _args: crate::args::SyncArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv, CommandInteraction};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;

    {
        let outcome = commands::sync::run(CommandInteraction::from_mode(opts.mode))?;
        let status = ExitStatus::from_shared(outcome.needs_attention(), outcome.needs_decision());
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}
