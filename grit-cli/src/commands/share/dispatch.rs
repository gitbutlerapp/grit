//! Dispatch for `grit share`.

pub fn dispatch(
    args: crate::args::ShareArgs,
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
    let ShareArgs { action } = args;
    match action {
        ShareAction::Add {
            branch,
            users,
            link,
            uses,
            expires,
            no_history,
            paths,
        } => emit_success(
            commands::share::add(
                &branch,
                &users,
                link,
                uses.as_deref(),
                expires,
                (no_history || !paths.is_empty()).then(|| api_view(paths)),
                CommandInteraction::from_mode(opts.mode),
            )?,
            opts,
            ctx,
        ),
        ShareAction::List { branch } => emit_success(
            commands::share::list(branch.as_deref(), CommandInteraction::from_mode(opts.mode))?,
            opts,
            ctx,
        ),
        ShareAction::Accept { link } => emit_success(
            commands::share::accept(&link, CommandInteraction::from_mode(opts.mode))?,
            opts,
            ctx,
        ),
        ShareAction::Remove { branch, users } => emit_success(
            commands::share::remove(&branch, &users, CommandInteraction::from_mode(opts.mode))?,
            opts,
            ctx,
        ),
        ShareAction::Leave { branch } => emit_success(
            commands::share::leave(&branch, CommandInteraction::from_mode(opts.mode))?,
            opts,
            ctx,
        ),
        ShareAction::Resolve {
            branch,
            option,
            name,
            with,
        } => {
            let outcome = commands::share::resolve(
                &branch,
                option.as_deref(),
                name.as_deref(),
                with.as_deref(),
                CommandInteraction::from_mode(opts.mode),
            )?;
            let status = ExitStatus::from_shared(false, outcome.needs_decision());
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        ShareAction::Revoke { branch, link } => {
            emit_success(commands::share::revoke(&branch, &link)?, opts, ctx)
        }
    }
}
