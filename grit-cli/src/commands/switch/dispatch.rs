//! Dispatch for `grit switch`.

pub fn dispatch(
    args: crate::args::SwitchArgs,
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
    let SwitchArgs {
        branch,
        start_point,
        create,
        track,
        no_track,
        detach,
        dry_run,
    } = args;
    {
        let outcome = commands::switch::run(
            branch,
            start_point,
            create,
            detach,
            track,
            no_track,
            dry_run,
        )?;
        crate::render::clone::render_path_collision_warning(
            &mut std::io::stderr(),
            opts.mode,
            &outcome.path_collisions,
        )?;
        emit_success(outcome, opts, ctx)
    }
}
