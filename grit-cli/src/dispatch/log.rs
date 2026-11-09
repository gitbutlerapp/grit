//! `grit log` CLI dispatch and presentation mapping.

use anyhow::{bail, Result};
use grit_lib::porcelain::log::{LogPresentationOptions, LogStdoutFormat};

use crate::args::{LogArgs, LogGroupByArg};
use crate::commands;
use crate::commands::log::TerminalLog;
use crate::commands::log::{LogDisplayOptions, LogGroupBy, LogPage};
use crate::context;
use crate::dispatch::{emit_success, is_broken_pipe, CommandEnv};
use crate::exit_status::ExitStatus;
use crate::output::OutputMode;
use crate::pager;
use crate::stdio;

/// Map CLI output mode and log display flags to library presentation options.
pub(crate) fn log_presentation(
    mode: OutputMode,
    display: LogDisplayOptions,
) -> LogPresentationOptions {
    LogPresentationOptions {
        format: match mode {
            OutputMode::Human => LogStdoutFormat::Human,
            OutputMode::Json => LogStdoutFormat::Json,
            OutputMode::Markdown => LogStdoutFormat::Markdown,
        },
        group_count_only: display.group_count_only,
    }
}

/// Dispatch parsed CLI args for `Log`.
pub fn dispatch(args: LogArgs, env: CommandEnv<'_>) -> Result<ExitStatus> {
    let CommandEnv {
        opts,
        ctx,
        process_env: _,
    } = env;
    let LogArgs {
        rev,
        pathspecs,
        cursor,
        format,
        oneline,
        decorate,
        group_by,
        count,
        graph,
        no_pager,
        graph_ascii,
        graph_unicode,
    } = args;
    if count && group_by.is_none() {
        bail!("`--count` requires `--group-by`");
    }
    let terminal = opts.mode.is_human() && std::io::IsTerminal::is_terminal(&std::io::stdout());
    let graph = graph || graph_unicode;
    let display = LogDisplayOptions {
        oneline,
        decorate,
        group_by: group_by.map(|g| match g {
            LogGroupByArg::Author => LogGroupBy::Author,
        }),
        group_count_only: count,
        graph,
        graph_ascii: graph_ascii && !graph_unicode,
        line_counts: terminal,
        columns: None,
        no_commits_in_repo: false,
    };
    let presentation = log_presentation(opts.mode, display);
    if format.patch_text && !opts.mode.is_human() {
        bail!("`--patch-text` cannot be used with --json or --markdown");
    }
    if !terminal {
        let plan = if opts.filter.is_none()
            && commands::log::listing_should_stream_incrementally(&rev, true)
        {
            commands::log::listing_stream_plan(&rev, &pathspecs, display, &cursor, &format)?
        } else {
            None
        };
        if let Some(plan) = plan {
            let stdout = std::io::stdout();
            let mut lock = stdout.lock();
            match commands::log::stream_listing(
                plan,
                rev,
                format,
                display,
                presentation,
                ctx,
                &mut lock,
            ) {
                Ok(()) => return Ok(ExitStatus::Success),
                Err(e) if is_broken_pipe(&e) => return Ok(ExitStatus::Success),
                Err(e) => return Err(e),
            }
        }
        let page = LogPage::Paged;
        let outcome =
            commands::log::run(rev, pathspecs, cursor, format, display, presentation, page)?;
        return emit_success(outcome, opts, ctx);
    }
    let view = if cursor.is_none() && rev.max_count().is_none() {
        TerminalLog::Stream(Box::new(commands::log::LogStream::start(
            rev, pathspecs, format, display,
        )?))
    } else {
        let page = LogPage::Paged;
        let outcome =
            commands::log::run(rev, pathspecs, cursor, format, display, presentation, page)?;
        TerminalLog::Listing(Box::new(outcome))
    };
    let mut pager = if no_pager {
        None
    } else {
        pager::Pager::start(context::discover().ok().as_ref())
    };
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    let w: &mut dyn std::io::Write = match pager.as_mut() {
        Some(pager) => pager,
        None => &mut stdout,
    };
    let result = stdio::with_sigpipe_ignored(|| view.render(w, ctx));
    match result {
        Err(e) if !is_broken_pipe(&e) => Err(e),
        _ => Ok(ExitStatus::Success),
    }
}
