//! Dispatch for `grit add`.

pub fn dispatch(
    args: crate::args::AddArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, finish_hunk_error, CommandEnv};

    use crate::progress;

    use anyhow::bail;
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let AddArgs {
        pathspecs,
        update,
        dry_run,
        intent_to_add,
        force,
        executable,
        non_executable,
        patch_file,
        hunks,
        lines,
        unified_context,
    } = args;
    {
        if unified_context.is_some() && hunks.is_empty() && lines.is_empty() {
            bail!("-U / --unified requires --hunks or --lines");
        }
        if let Some(file) = patch_file {
            if !hunks.is_empty() || !lines.is_empty() {
                bail!("--patch-file cannot be used with --hunks or --lines");
            }
            if executable || non_executable {
                bail!("--patch-file cannot be used with --executable or --non-executable");
            }
            if force {
                bail!("--patch-file cannot be used with --force");
            }
            if update {
                bail!("--patch-file cannot be used with --update");
            }
            if intent_to_add {
                bail!("--patch-file cannot be used with --intent-to-add");
            }
            if !pathspecs.paths.is_empty() || pathspecs.pathspec_from_file.is_some() {
                bail!("--patch-file cannot be used with path arguments");
            }
            emit_success(commands::add::run_patch_file(&file, dry_run)?, opts, ctx)
        } else if !hunks.is_empty() || !lines.is_empty() {
            if executable || non_executable {
                bail!("--hunks and --lines cannot be used with --executable or --non-executable");
            }
            if force {
                bail!("--hunks and --lines cannot be used with --force");
            }
            if update {
                bail!("--hunks and --lines cannot be used with --update");
            }
            if intent_to_add {
                bail!("--hunks and --lines cannot be used with --intent-to-add");
            }
            match commands::add::run_hunks(&pathspecs, &hunks, &lines, unified_context, dry_run) {
                Ok(outcome) => emit_success(outcome, opts, ctx),
                Err(err) => {
                    let code = commands::add::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        } else {
            emit_success(
                commands::add::run(
                    &pathspecs,
                    update,
                    dry_run,
                    intent_to_add,
                    force,
                    executable,
                    non_executable,
                    &mut progress::TerminalProgress::new(opts.mode.is_human()),
                )?,
                opts,
                ctx,
            )
        }
    }
}
