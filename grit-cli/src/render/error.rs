//! Render command failures for human, `--json`, and `--markdown` output modes.

use std::ffi::OsString;
use std::io::Write;

use anyhow::Error;
use clap::Error as ClapError;
use grit_lib::command_runner::hooks::HookError;
use serde_json::Value;

use crate::error_kind::ErrorKind;
use crate::json_error::{structured_json_payload, StructuredCliError, StructuredJsonError};
use crate::json_filter::apply_json_filter;
use crate::output::OutputOptions;
use crate::render::list;
use crate::render::OutputMode;
use crate::stdio;

/// Human-readable summary for a command failure (also used as JSON `error` when unstructured).
#[must_use]
pub fn human_message(err: &Error) -> String {
    crate::error_map::classify(err).human
}

/// Format a clap usage error for `--json` / `--markdown`: keep missing-argument detail, omit Usage/help.
#[must_use]
pub fn clap_failure_message(err: &ClapError) -> String {
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

/// First error line from clap for human output, optionally trimmed for a migration hint.
#[must_use]
pub fn clap_human_message(err: &ClapError, hint: Option<&str>) -> String {
    let mut lines: Vec<String> = err
        .to_string()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_owned)
        .collect();
    lines.retain(|line| !is_misleading_clap_tip(line));
    if lines.is_empty() {
        return "invalid arguments".to_owned();
    }
    let first = lines[0]
        .strip_prefix("error:")
        .or_else(|| lines[0].strip_prefix("error: "))
        .unwrap_or(&lines[0])
        .trim()
        .to_owned();
    if hint.is_some() {
        first
    } else {
        lines.join("\n")
    }
}

/// Usage lines from a clap failure, rewritten for the subcommand name the user typed.
#[must_use]
pub fn clap_usage_lines(argv: &[OsString], err: &ClapError) -> Vec<String> {
    err.to_string()
        .lines()
        .filter(|line| line.trim_start().starts_with("Usage:"))
        .map(|line| rewrite_usage_line(argv, line))
        .collect()
}

/// Usage and help trailer after a mapped clap failure (human mode only).
pub fn write_clap_trailer(argv: &[OsString], err: &ClapError) {
    for line in clap_usage_lines(argv, err) {
        let _ = stdio::io_result(writeln!(std::io::stderr(), "{line}"));
    }
    for line in err.to_string().lines() {
        if line.trim_start().starts_with("For more information") {
            let _ = stdio::io_result(writeln!(std::io::stderr(), "{line}"));
        }
    }
}

fn rewrite_usage_line(argv: &[OsString], line: &str) -> String {
    let Some(display) = user_switch_display(argv) else {
        return line.to_owned();
    };
    line.replace("Usage: grit switch", &format!("Usage: grit {display}"))
        .replace("Usage: grit co", &format!("Usage: grit {display}"))
        .replace("Usage: grit checkout", &format!("Usage: grit {display}"))
}

fn is_misleading_clap_tip(line: &str) -> bool {
    let trimmed = line.trim_start();
    (trimmed.starts_with("tip:") && trimmed.contains("to pass") && trimmed.contains("use '--"))
        || (trimmed.starts_with("tip:") && trimmed.contains("as text"))
        || (trimmed.starts_with("tip:") && trimmed.contains("similar subcommand"))
}

fn user_switch_display(argv: &[OsString]) -> Option<&'static str> {
    for arg in argv.iter().skip(1) {
        if arg == "checkout" {
            return Some("checkout");
        }
        if arg == "co" {
            return Some("co");
        }
    }
    None
}

/// Report a command failure on stdout or stderr according to [`OutputOptions::mode`].
pub fn emit(err: &Error, opts: &OutputOptions) {
    if let Err(filter_err) = opts.validate() {
        let _ = stdio::io_result(writeln!(std::io::stderr(), "error: {filter_err:#}"));
        return;
    }
    let human = human_message(err);
    let hook_output = crate::json_error::hook_error_from_chain(err).and_then(HookError::output);
    let hint = structured_json_payload(err).and_then(|payload| payload.hint);
    match opts.mode {
        OutputMode::Human => emit_human(&human, hint.as_deref(), hook_output),
        OutputMode::Markdown => emit_markdown(&human, hint.as_deref(), hook_output),
        OutputMode::Json => emit_json(err, opts, &human),
    }
}

fn emit_human(
    human: &str,
    hint: Option<&str>,
    hook_output: Option<&grit_lib::command_runner::hooks::HookOutput>,
) {
    if let Some(output) = hook_output {
        let mut stderr = std::io::stderr().lock();
        let _ = stdio::io_result(stderr.write_all(&output.stdout));
        let _ = stdio::io_result(stderr.write_all(&output.stderr));
    }
    if human.starts_with("fatal:") || human.starts_with("error:") {
        let _ = stdio::io_result(writeln!(std::io::stderr(), "{human}"));
    } else {
        let _ = stdio::io_result(writeln!(std::io::stderr(), "error: {human}"));
    }
    if let Some(hint) = hint {
        let _ = stdio::io_result(writeln!(std::io::stderr(), "hint: {hint}"));
    }
}

fn emit_markdown(
    human: &str,
    hint: Option<&str>,
    hook_output: Option<&grit_lib::command_runner::hooks::HookOutput>,
) {
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = stdio::io_result(writeln!(lock, "**Error:** {human}"));
    if let Some(hint) = hint {
        let _ = stdio::io_result(writeln!(lock, "\n**Hint:** {hint}"));
    }
    if let Some(output) = hook_output {
        let _ = list::render_hook_output(
            &mut lock,
            &String::from_utf8_lossy(&output.stdout),
            &String::from_utf8_lossy(&output.stderr),
        );
    }
}

fn emit_json(err: &Error, opts: &OutputOptions, human: &str) {
    let payload: Value = if let Some(structured) = structured_json_payload(err) {
        serde_json::to_value(structured).unwrap_or_else(|_| serde_json::json!({ "error": human }))
    } else if let Some(expr) = opts.filter.as_deref() {
        let full = serde_json::json!({ "error": human });
        match apply_json_filter(&full, expr) {
            Ok(filtered) => filtered,
            Err(filter_err) => serde_json::json!({ "error": format!("{filter_err:#}") }),
        }
    } else {
        serde_json::json!({ "error": human })
    };
    let stdout = std::io::stdout();
    let mut lock = stdout.lock();
    let _ = stdio::io_result(writeln!(lock, "{payload}"));
}

/// Write a single human-mode `error:` line when no output options are available (top-level handler).
pub fn emit_fatal_line(err: &Error) {
    let _ = stdio::io_result(writeln!(std::io::stderr(), "error: {err:#}"));
}

/// Build a structured usage [`Error`] from a clap parse failure.
#[must_use]
pub fn structured_clap_failure(err: &ClapError) -> Error {
    let message = clap_failure_message(err);
    Error::new(StructuredCliError {
        payload: StructuredJsonError::new(message.clone(), ErrorKind::Usage),
        human: message,
    })
}
