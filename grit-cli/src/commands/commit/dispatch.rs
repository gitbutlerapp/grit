//! Dispatch for `grit commit`.

pub fn dispatch(
    args: crate::args::CommitArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success_quiet, finish_hunk_error, CommandEnv};

    use crate::message_input;

    use crate::patch_selection;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let CommitArgs {
        commit_message,
        message_input,
        quiet,
        author,
        author_date,
        trailers,
        signoff,
        reset_author,
        reuse_message,
        gpg_sign,
        no_gpg_sign,
        no_verify,
        all,
        staged,
        amend,
        allow_empty,
        dry_run,
        patch_file,
        hunks,
        lines,
        paths,
    } = args;
    {
        let message_flag = message_input::joined_message_from_flags(&message_input.message);
        let message_file = message_input.message_file.clone();
        let author_override = author.author.clone();
        let message = commit_message;
        patch_selection::ensure_single_patch_mode(
            patch_file.is_some(),
            !hunks.is_empty(),
            !lines.is_empty(),
        )?;
        if let Some(file) = patch_file {
            emit_success_quiet(
                commands::commit::run_patch_file(
                    message.or(message_flag),
                    message_file,
                    author_override,
                    author_date,
                    trailers,
                    signoff,
                    reset_author,
                    reuse_message,
                    gpg_sign,
                    no_gpg_sign,
                    no_verify,
                    dry_run,
                    amend,
                    allow_empty,
                    &file,
                )?,
                &quiet,
                opts,
                ctx,
            )
        } else if !hunks.is_empty() || !lines.is_empty() {
            match commands::commit::run_hunks(
                message.or(message_flag),
                message_file,
                author_override,
                author_date,
                trailers,
                signoff,
                reset_author,
                reuse_message,
                gpg_sign,
                no_gpg_sign,
                no_verify,
                dry_run,
                amend,
                allow_empty,
                &paths,
                &hunks,
                &lines,
            ) {
                Ok(outcome) => emit_success_quiet(outcome, &quiet, opts, ctx),
                Err(err) => {
                    let code = commands::commit::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        } else {
            emit_success_quiet(
                commands::commit::run(
                    message.or(message_flag),
                    message_file,
                    author_override,
                    author_date,
                    trailers,
                    signoff,
                    reset_author,
                    reuse_message,
                    gpg_sign,
                    no_gpg_sign,
                    no_verify,
                    dry_run,
                    amend,
                    allow_empty,
                    all,
                    staged,
                    paths,
                    &mut quiet.progress(opts.mode.is_human()),
                )?,
                &quiet,
                opts,
                ctx,
            )
        }
    }
}
