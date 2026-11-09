//! `grit diff` dispatch from the CLI driver (output mode, patch streaming, exit status).

use crate::args::DiffArgs;
use crate::dispatch::CommandEnv;
use crate::exit_status::ExitStatus;
use crate::json_error;
use crate::output::{self, OutputMode, OutputOptions, RenderContext};

use super::{run, DiffRunContext, DiffRunOptions, PatchBodies};

/// Parse [`DiffArgs`] and run `grit diff`.
pub fn dispatch(args: DiffArgs, env: CommandEnv<'_>) -> anyhow::Result<ExitStatus> {
    let CommandEnv { opts, ctx, .. } = env;
    let DiffArgs {
        no_index,
        staged,
        unstaged,
        against,
        exit_code,
        quiet,
        commits,
        pathspecs,
        format,
        line_format,
    } = args;
    let run_opts = DiffRunOptions {
        no_index,
        staged,
        unstaged,
        against,
        commits,
        pathspecs,
        format,
        line_format,
        quiet,
    };
    execute(no_index, exit_code, quiet, run_opts, opts, ctx)
}

/// Run `grit diff` and map the result to process exit status.
pub fn execute(
    no_index: bool,
    exit_code: bool,
    quiet: bool,
    run_opts: DiffRunOptions,
    opts: &OutputOptions,
    ctx: &RenderContext,
) -> Result<ExitStatus, anyhow::Error> {
    let patch_text = run_opts.format.patch_text;
    if patch_text && opts.mode != OutputMode::Human {
        anyhow::bail!("`--patch-text` cannot be used with --json or --markdown");
    }
    let wants_exit_code = exit_code || quiet || no_index;
    let patch_bodies = if quiet || patch_text {
        PatchBodies::MetadataOnly
    } else if opts.mode == OutputMode::Json {
        PatchBodies::Collect
    } else {
        PatchBodies::Stream
    };
    let run_ctx = DiffRunContext {
        word_level_inline: matches!(opts.mode, OutputMode::Human)
            && super::diff_word_level_inline_for_output(),
        emit_json_hunk_ids: opts.mode == OutputMode::Json,
        json_stream: opts.mode == OutputMode::Json && opts.filter.is_none(),
        patch_bodies,
        render: RenderContext {
            now: ctx.now,
            mode: opts.mode,
            color: ctx.color,
        },
    };
    match run(run_opts, run_ctx) {
        Ok(outcome) => {
            let status = if no_index || wants_exit_code {
                ExitStatus::from_diff_outcome(&outcome)
            } else {
                ExitStatus::Success
            };
            if !quiet && !patch_text && !(outcome.body_streamed && opts.mode == OutputMode::Json) {
                output::emit(outcome, opts, ctx)?;
            }
            Ok(status)
        }
        Err(err) => {
            let err = json_error::ensure_structured(err);
            output::emit_error(&err, opts);
            Ok(ExitStatus::diff_command_error(no_index || wants_exit_code))
        }
    }
}
