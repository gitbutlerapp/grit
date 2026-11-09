//! Dispatch for `grit show`.

pub fn dispatch(
    args: crate::args::ShowArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;

    use crate::dispatch::{emit_success, CommandEnv};

    use crate::output::OutputMode;

    use anyhow::bail;
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let ShowArgs {
        objects,
        diff_merges,
        pathspecs,
        format,
    } = args;
    {
        if format.patch_text && opts.mode != OutputMode::Human {
            bail!("`--patch-text` cannot be used with --json or --markdown");
        }
        let diff_merges = grit_lib::diff::DiffMerges::parse(&diff_merges).ok_or_else(|| {
            anyhow::anyhow!(
                "unknown --diff-merges mode '{diff_merges}' (expected first-parent or combined)"
            )
        })?;
        emit_success(
            commands::show::run(commands::show::ShowRunOptions {
                objects,
                pathspecs,
                format,
                word_level_inline: matches!(opts.mode, crate::output::OutputMode::Human)
                    && commands::diff::diff_word_level_inline_for_output(),
                diff_merges,
                include_refs: matches!(opts.mode, crate::output::OutputMode::Json),
            })?,
            opts,
            ctx,
        )
    }
}
