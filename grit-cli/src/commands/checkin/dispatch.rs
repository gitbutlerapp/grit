//! Dispatch for `grit checkin` and `grit integrate`.

/// Dispatch parsed CLI args for `Checkin`.
pub fn dispatch(
    args: crate::args::CheckinArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv, CommandInteraction};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let CheckinArgs { branches, force } = args;
    {
        let outcome =
            commands::checkin::run(&branches, force, CommandInteraction::from_mode(opts.mode))?;
        let status = ExitStatus::from_push_rejected(outcome.refused);
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}

/// Dispatch parsed CLI args for `Integrate`.
pub fn dispatch_integrate(
    args: crate::args::IntegrateArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv, CommandInteraction};
    use crate::exit_status::ExitStatus;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let IntegrateArgs { branch, without } = args;
    {
        let outcome = commands::checkin::integrate(
            &branch,
            &without,
            CommandInteraction::from_mode(opts.mode),
        )?;
        let status = ExitStatus::from_integrate(&outcome);
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}
