//! `grit` — a small, opinionated command line interface backed by `grit-lib`.
//!
//! `grit` deliberately does not mirror Git's UX. It favors a single obvious way
//! to do the common thing, plain-language output, and a status screen that
//! doubles as the home base: running `grit` with no arguments shows you where you
//! are, what's changed, and what to do next.

mod args;
mod branch_list_args;
mod clap_value_args;
mod cli_command;
#[cfg(test)]
mod cli_contract;
mod cli_messages;
#[cfg(test)]
mod cli_test_stack;
mod commands;
mod commits_json;
mod conflict_style;
mod context;
mod dates;
mod diagnostics;
mod diff_args;
mod diff_format;
mod diff_render;
mod dispatch;
mod embedded_gitlink;
mod error_hints;
mod error_kind;
mod error_map;
#[cfg(test)]
mod error_map_contract;
mod exit_status;
mod global_args;
mod grit_trace;
mod interrupt;
mod json_error;
mod json_filter;
#[cfg(test)]
mod json_schema;
mod json_to_jaq;
mod limit_args;
mod log_display;
mod markdown;
mod merge_strategy_args;
mod message_input;
mod operation_output;
mod output;
mod pager;
mod patch_selection;
mod pathspec_args;
mod progress;
mod quiet_args;
mod ref_name_messages;
mod render;
mod rev_select;
mod revision_errors;
mod stdio;
mod stream_commit_list;
mod terminal_progress;
mod ui;
mod unborn_errors;
mod usage_hints;

#[cfg(test)]
mod contract_tests;
#[cfg(test)]
mod help_output_snapshots;
#[cfg(test)]
mod render_outcome_snapshots;

pub(crate) use args::Cli;
pub(crate) use args::RemoteRefSortArg;
pub(crate) use args::{
    normalize_log_limit_argv, normalize_tag_verify_argv, parse_cli,
};

use std::ffi::OsString;

use anyhow::Result;
use grit_lib::command_runner::hooks::HookStdio;

use dispatch::{
    dispatch, reject_non_human_plumbing, talks_to_remotes, validate_cli, writes_objects, CommandEnv,
};
use exit_status::ExitStatus;
use output::{OutputMode, OutputOptions, RenderContext};

/// Whether clap stopped to print help, which needs [`cli_command::full_cli_command`] to show
/// each subcommand's own description.
fn is_help_request(err: &clap::Error) -> bool {
    matches!(
        err.kind(),
        clap::error::ErrorKind::DisplayHelp
            | clap::error::ErrorKind::DisplayHelpOnMissingArgumentOrSubcommand
    )
}

/// Format a clap usage error for `--json`: keep missing-argument detail, omit Usage/help.
fn clap_json_error_message(err: &clap::Error) -> String {
    let raw = err.to_string();
    let mut lines = raw.lines();
    let Some(first) = lines.next() else {
        return "invalid arguments".to_owned();
    };
    let first = first
        .strip_prefix("error: ")
        .or_else(|| first.strip_prefix("error:"))
        .unwrap_or(first)
        .trim();
    let mut out = vec![first.to_owned()];
    for line in lines {
        let trimmed = line.trim();
        if trimmed.starts_with("Usage:") || trimmed.starts_with("For more information") {
            break;
        }
        if trimmed.is_empty() {
            if out.len() > 1 {
                break;
            }
            continue;
        }
        out.push(line.to_owned());
    }
    let message = out.join("\n").trim().to_owned();
    if message.is_empty() {
        "invalid arguments".to_owned()
    } else {
        message
    }
}

fn clap_failure_output_options(argv: &[OsString]) -> OutputOptions {
    let mode = if argv.iter().any(|a| a == "--json") {
        OutputMode::Json
    } else if argv.iter().any(|a| a == "--markdown") {
        OutputMode::Markdown
    } else {
        OutputMode::Human
    };
    OutputOptions {
        mode,
        filter: extract_filter_flag_from(argv),
    }
}

fn extract_filter_flag_from(argv: &[OsString]) -> Option<String> {
    let mut args = argv.iter();
    while let Some(arg) = args.next() {
        if arg == "--filter" {
            return args.next().and_then(|arg| arg.to_str()).map(str::to_owned);
        }
        if let Some(expr) = arg.to_str().and_then(|arg| arg.strip_prefix("--filter=")) {
            return Some(expr.to_owned());
        }
    }
    None
}

fn main() {
    stdio::configure();
    grit_trace::maybe_init_from_env();
    let status = match run() {
        Ok(status) => status,
        Err(err) => {
            eprintln!("error: {err:#}");
            ExitStatus::Failure
        }
    };
    std::process::exit(status.code());
}

fn run() -> Result<ExitStatus, anyhow::Error> {
    let parse_argv = normalize_tag_verify_argv(normalize_log_limit_argv(
        diff_args::normalize_diff_argv(std::env::args_os().collect()),
    ));
    let cli = match parse_cli(&parse_argv).and_then(|cli| {
        validate_cli(&cli)?;
        Ok(cli)
    }) {
        Ok(cli) => cli,
        Err(err) if is_help_request(&err) => {
            let bin_name = parse_argv
                .first()
                .and_then(|argv0| std::path::Path::new(argv0).file_name())
                .map_or_else(|| "grit".into(), |name| name.to_string_lossy());
            let err = cli_command::full_cli_command(&bin_name)
                .try_get_matches_from(&parse_argv)
                .err()
                .unwrap_or(err);
            let _ = err.print();
            return Ok(ExitStatus::from_clap_code(err.exit_code()));
        }
        Err(err) if err.use_stderr() => {
            let opts = clap_failure_output_options(&parse_argv);
            if let Some(structured) = usage_hints::clap_failure(&parse_argv, &err) {
                output::emit_error(&structured, &opts);
                if opts.mode == OutputMode::Human {
                    usage_hints::eprint_clap_trailer(&parse_argv, &err);
                }
                return Ok(ExitStatus::from_clap_code(err.exit_code()));
            }
            if opts.mode != OutputMode::Human {
                let message = clap_json_error_message(&err);
                let payload = json_error::StructuredJsonError::new(
                    message.clone(),
                    error_kind::ErrorKind::Usage,
                );
                output::emit_error(
                    &anyhow::Error::new(json_error::StructuredCliError {
                        payload,
                        human: message,
                    }),
                    &opts,
                );
                return Ok(ExitStatus::from_clap_code(err.exit_code()));
            }
            let _ = err.print();
            return Ok(ExitStatus::from_clap_code(err.exit_code()));
        }
        Err(err) => {
            let _ = err.print();
            return Ok(ExitStatus::from_clap_code(err.exit_code()));
        }
    };
    // Parse failures use the local argv; runtime hints can take ownership now.
    usage_hints::capture_argv(parse_argv);
    let opts = OutputOptions {
        mode: if cli.json {
            OutputMode::Json
        } else if cli.markdown {
            OutputMode::Markdown
        } else {
            OutputMode::Human
        },
        filter: cli.filter.clone(),
    };
    context::set_thread_policy(cli.threads);
    // Automatic pool sizes assume the machine is ours; share them with other grit processes
    // so many at once (agents, say) do not each run a full pool. `--threads N` is kept as asked.
    if cli.threads.is_none_or(|n| n == 0) {
        grit_lib::parallel::share_worker_budget_across_processes(true);
    }
    // Humans see hook output live; `--json` / `--markdown` capture it into the result instead.
    context::set_hook_stdio(if opts.mode == OutputMode::Human {
        HookStdio::Inherit
    } else {
        HookStdio::Capture
    });
    if let Err(err) = opts.validate() {
        if opts.mode == OutputMode::Json {
            output::emit_error(&err, &opts);
        } else {
            eprintln!("error: {err:#}");
        }
        return Ok(ExitStatus::Failure);
    }
    if let Err(err) = reject_non_human_plumbing(&cli, &opts) {
        output::emit_error(&err, &opts);
        return Ok(ExitStatus::Failure);
    }
    if let Err(err) = cli.repo.apply() {
        output::emit_error(&err, &opts);
        return Ok(ExitStatus::for_error(&err));
    }
    let env = context::freeze_environment();
    let render_ctx = RenderContext {
        now: context::wall_clock_now(&env),
        mode: opts.mode,
        color: output::stdout_color_enabled(),
    };
    let command_env = CommandEnv {
        opts: &opts,
        ctx: &render_ctx,
        process_env: &env,
    };
    let auto_maintenance = cli.command.as_ref().is_some_and(writes_objects);
    let result = if cli.command.as_ref().is_some_and(talks_to_remotes) {
        stdio::with_sigpipe_ignored(|| dispatch(cli, command_env))
    } else {
        dispatch(cli, command_env)
    };
    match result {
        Ok(ExitStatus::Success) if auto_maintenance => {
            commands::gc::run_auto_after_command(opts.mode, render_ctx.now);
            Ok(ExitStatus::Success)
        }
        Ok(status) => Ok(status),
        Err(err) => {
            // Structuring can replace a typed error that also decides the exit code (a clone
            // that could not read the remote exits 128), so the more specific status wins.
            let status = ExitStatus::for_error(&err);
            let err = json_error::ensure_structured(err);
            output::emit_error(&err, &opts);
            Ok(match status {
                ExitStatus::Failure => ExitStatus::for_error(&err),
                specific => specific,
            })
        }
    }
}
