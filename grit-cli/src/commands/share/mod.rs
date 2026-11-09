//! `grit share` — work on a branch together with other users of a grit
//! hosting server such as grithub.
//!
//! A shared branch is in every member's space, and each member has their
//! own head of it: only you move yours, so nobody's push can overwrite
//! anyone else's work, and pushing your head after an amend or rebase needs
//! no `--force`. The other members' heads are fetched beside yours as
//! `<remote>/shared/<branch>/<user>`.
//!
//! - `grit share add <branch> [<user>...] [--link]` shares a branch of your
//!   space (pushing it first when it isn't there yet), adds people, or makes
//!   a share link anyone signed in can accept.
//! - `grit share list [<branch>]` shows your shares: who is on each, where
//!   their heads are, and how each compares with your branch here.
//! - `grit share accept <link>` accepts a link and fetches the branch (or
//!   clones your copy when you aren't in a clone of the project).
//! - `grit share remove <branch> [<user>...]` removes people, or ends the
//!   share; `grit share leave <branch>` leaves it; `grit share revoke
//!   <branch> <link-id>` revokes a link.

pub use crate::render::share::*;

pub mod divergence;
pub mod integrated;
pub mod ops;

pub(crate) use divergence::{auto_combine, compare, explain};
pub(crate) use integrated::integrated_on;
pub use ops::{accept, add, leave, list, parse_uses, remove, resolve, revoke};

/// Dispatch parsed CLI args for `Share`.
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
        ShareAction::Accept { link } => {
            emit_success(commands::share::accept(&link, opts.mode)?, opts, ctx)
        }
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
                opts.mode,
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
