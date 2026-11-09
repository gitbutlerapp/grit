//! Dispatch for `grit log`.

pub fn dispatch(
    args: crate::args::LogArgs,
    env: crate::dispatch::CommandEnv<'_>,
) -> anyhow::Result<crate::exit_status::ExitStatus> {
    use crate::args::*;
    use crate::commands;
    use crate::context;
    use crate::dispatch::{emit_success, is_broken_pipe, CommandEnv};
    use crate::exit_status::ExitStatus;

    use crate::output::OutputMode;
    use crate::pager;

    use crate::stdio;
    use anyhow::bail;
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
    {
        if count && group_by.is_none() {
            bail!("`--count` requires `--group-by`");
        }
        // A person at a terminal gets the whole history through the pager; scripts, agents
        // and pipes get pages with a `--cursor` token.
        let terminal =
            opts.mode == OutputMode::Human && std::io::IsTerminal::is_terminal(&std::io::stdout());
        let graph = graph || graph_unicode;
        let display = commands::log::LogDisplayOptions {
            oneline,
            decorate,
            group_by: group_by.map(|g| match g {
                LogGroupByArg::Author => commands::log::LogGroupBy::Author,
            }),
            group_count_only: count,
            graph,
            graph_ascii: graph_ascii && !graph_unicode,
            line_counts: terminal,
            columns: None,
            no_commits_in_repo: false,
        };
        if format.patch_text && opts.mode != OutputMode::Human {
            bail!("`--patch-text` cannot be used with --json or --markdown");
        }
        let emit = match opts.mode {
            OutputMode::Human => commands::log::LogEmitKind::Human,
            OutputMode::Json => commands::log::LogEmitKind::Json,
            OutputMode::Markdown => commands::log::LogEmitKind::Markdown,
        };
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
                match crate::render::log_stream::stream_listing(
                    plan, rev, format, display, emit, ctx, &mut lock,
                ) {
                    Ok(()) => return Ok(ExitStatus::Success),
                    Err(e) if is_broken_pipe(&e) => return Ok(ExitStatus::Success),
                    Err(e) => return Err(e),
                }
            }
            let page = commands::log::LogPage::Paged;
            let outcome = commands::log::run(rev, pathspecs, cursor, format, display, emit, page)?;
            return emit_success(outcome, opts, ctx);
        }
        // With no `-n` or `--cursor`, the whole history, walked a batch at a time.
        let view = if cursor.is_none() && rev.max_count().is_none() {
            commands::log::TerminalLog::Stream(Box::new(commands::log::LogStream::start(
                rev, pathspecs, format, display,
            )?))
        } else {
            let page = commands::log::LogPage::Paged;
            commands::log::TerminalLog::Listing(Box::new(commands::log::run(
                rev, pathspecs, cursor, format, display, emit, page,
            )?))
        };
        // Start the pager only once the first batch is walked, so errors and Ctrl-C during
        // a slow start behave as usual.
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
}
