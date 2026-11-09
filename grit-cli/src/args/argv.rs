//! argv rewriting and CLI parsing.

use std::ffi::OsString;

use crate::cli_command;
use crate::diff_args;

use super::Cli;

/// True when this token is a `tag` subcommand flag that consumes the next argv word.
///
/// Keep this list in sync with value-taking [`Cli`] global options (`--filter`, `--threads`).
fn tag_verify_subarg_takes_value(arg: &str) -> bool {
    matches!(arg, "--filter" | "--threads")
}

/// Rewrite `log -1` / `log -3` (Git habit) into `-n` / `--limit`.
pub fn normalize_log_limit_argv(args: Vec<OsString>) -> Vec<OsString> {
    let Some(log_idx) = diff_args::subcommand_index(&args).filter(|&i| args[i] == "log") else {
        return args;
    };
    let sub = &args[log_idx + 1..];
    if sub.iter().any(|a| a == "-n" || a == "--limit") {
        return args;
    }
    let mut out = args[..=log_idx].to_vec();
    let mut i = log_idx + 1;
    while i < args.len() {
        if args[i] == "--" {
            out.extend_from_slice(&args[i..]);
            break;
        }
        if let Some(t) = args[i].to_str() {
            if t.len() > 1 && t.starts_with('-') && t[1..].chars().all(|c| c.is_ascii_digit()) {
                out.push("-n".into());
                out.push(t[1..].into());
            } else {
                out.push(args[i].clone());
            }
        } else {
            out.push(args[i].clone());
        }
        i += 1;
    }
    out
}

/// Route verify tag names to `verify_names` by inserting `--` before bare arguments.
pub fn normalize_tag_verify_argv(mut args: Vec<OsString>) -> Vec<OsString> {
    let Some(tag_idx) = diff_args::subcommand_index(&args).filter(|&i| args[i] == "tag") else {
        return args;
    };
    let sub = &args[tag_idx + 1..];
    if !sub.iter().any(|a| a == "--verify" || a == "-v") {
        return args;
    }
    if sub.iter().any(|arg| arg == "--") {
        return args;
    }
    let mut flags = Vec::new();
    let mut names = Vec::new();
    let mut i = 0;
    while i < sub.len() {
        let arg = &sub[i];
        if arg.to_str().is_some_and(tag_verify_subarg_takes_value) {
            flags.push(arg.clone());
            if i + 1 < sub.len() {
                flags.push(sub[i + 1].clone());
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }
        if arg.as_encoded_bytes().starts_with(b"-") {
            flags.push(arg.clone());
            i += 1;
            continue;
        }
        names.push(arg.clone());
        i += 1;
    }
    if names.is_empty() {
        return args;
    }
    args.truncate(tag_idx + 1);
    args.extend(flags);
    args.push("--".into());
    args.extend(names);
    args
}

/// [`Cli::try_parse_from`] over [`cli_command::parse_command`]'s tree.
pub fn parse_cli(argv: &[OsString]) -> Result<Cli, clap::Error> {
    use clap::{CommandFactory, FromArgMatches};
    let mut matches = cli_command::parse_command(argv).try_get_matches_from(argv)?;
    Cli::from_arg_matches_mut(&mut matches).map_err(|err| err.format(&mut Cli::command()))
}
