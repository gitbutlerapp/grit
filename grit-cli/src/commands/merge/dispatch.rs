//! Dispatch for `grit merge`.

pub fn dispatch(
    args: crate::args::MergeArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};
    use crate::exit_status::ExitStatus;

    use crate::progress;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let MergeArgs {
        branch,
        abort,
        continue_merge,
        check,
        dry_run,
        ff_only,
        no_ff,
        no_commit,
        squash,
        allow_unrelated_histories,
        no_verify,
        strategy,
        message,
    } = args;
    {
        let outcome = commands::merge::run(
            branch.as_deref(),
            abort,
            continue_merge,
            check,
            dry_run,
            commands::merge::MergeRunOptions {
                ff_only,
                no_ff,
                no_commit,
                squash,
                allow_unrelated_histories,
                no_verify,
                strategy,
                message,
            },
            &mut progress::TerminalProgress::new(opts.mode.is_human()),
        )?;
        let status = ExitStatus::from_merge_outcome(&outcome);
        emit_success(outcome, opts, ctx)?;
        Ok(status)
    }
}
