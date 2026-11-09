//! Dispatch for `grit stash`.

pub fn dispatch(
    args: crate::args::StashArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, finish_hunk_error, CommandEnv};
    use crate::exit_status::ExitStatus;

    use crate::progress;

    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let StashArgs {
        message,
        include_untracked,
        hunks,
        lines,
        dry_run,
        staged,
        keep_index,
        stash_limit,
        pathspecs,
        action,
    } = args;
    match action {
        None => {
            let outcome = commands::stash::run_push(commands::stash::StashPushRunArgs {
                message,
                include_untracked,
                staged,
                keep_index,
                hunks: &hunks,
                lines: &lines,
                dry_run,
                limit: stash_limit,
                pathspec_args: &pathspecs,
                progress: &mut progress::TerminalProgress::new(opts.mode.is_human()),
            });
            match outcome {
                Ok(o) => emit_success(o, opts, ctx),
                Err(err) => {
                    let code = commands::stash::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        }
        Some(StashAction::Push {
            message: push_message,
            include_untracked: push_untracked,
            hunks: push_hunks,
            lines: push_lines,
            dry_run: push_dry_run,
            staged: push_staged,
            keep_index: push_keep_index,
            pathspecs: push_pathspecs,
        }) => {
            let merged_hunks = if push_hunks.is_empty() {
                hunks
            } else {
                push_hunks
            };
            let merged_lines = if push_lines.is_empty() {
                lines
            } else {
                push_lines
            };
            let merged_pathspecs =
                if push_pathspecs.paths.is_empty() && push_pathspecs.pathspec_from_file.is_none() {
                    pathspecs
                } else {
                    push_pathspecs
                };
            let outcome = commands::stash::run_push(commands::stash::StashPushRunArgs {
                message: push_message.or(message),
                include_untracked: include_untracked || push_untracked,
                staged: staged || push_staged,
                keep_index: keep_index || push_keep_index,
                hunks: &merged_hunks,
                lines: &merged_lines,
                dry_run: dry_run || push_dry_run,
                limit: stash_limit,
                pathspec_args: &merged_pathspecs,
                progress: &mut progress::TerminalProgress::new(opts.mode.is_human()),
            });
            match outcome {
                Ok(o) => emit_success(o, opts, ctx),
                Err(err) => {
                    let code = commands::stash::hunks_exit_code(&err);
                    Ok(finish_hunk_error(err, opts, code))
                }
            }
        }
        Some(StashAction::List) => emit_success(commands::stash::run_list()?, opts, ctx),
        Some(StashAction::Show {
            index,
            stat: _,
            patch,
            include_untracked,
        }) => emit_success(
            commands::stash::run_show(index, patch, include_untracked)?,
            opts,
            ctx,
        ),
        Some(StashAction::Branch { name, index }) => {
            let outcome = commands::stash::run_branch(index, &name)?;
            let status = ExitStatus::from_stash_conflicts(outcome.conflicts);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Some(StashAction::Clear { force, dry_run }) => {
            let outcome = commands::stash::run_clear(force, dry_run)?;
            emit_success(outcome, opts, ctx)?;
            if !force {
                Ok(ExitStatus::Failure)
            } else {
                Ok(ExitStatus::Success)
            }
        }
        Some(StashAction::Apply {
            index,
            restore_index,
        }) => {
            let outcome = commands::stash::run_apply(
                index,
                restore_index,
                &mut progress::TerminalProgress::new(opts.mode.is_human()),
            )?;
            let status = ExitStatus::from_stash_conflicts(outcome.conflicts);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Some(StashAction::Pop {
            index,
            restore_index,
        }) => {
            let outcome = commands::stash::run_pop(
                index,
                restore_index,
                &mut progress::TerminalProgress::new(opts.mode.is_human()),
            )?;
            let status = ExitStatus::from_stash_conflicts(outcome.conflicts);
            emit_success(outcome, opts, ctx)?;
            Ok(status)
        }
        Some(StashAction::Drop { index }) => {
            emit_success(commands::stash::run_drop(index)?, opts, ctx)
        }
    }
}
