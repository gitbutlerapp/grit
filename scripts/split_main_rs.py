#!/usr/bin/env python3
"""Split grit-cli/src/main.rs into args/, dispatch.rs, and contract_tests.rs.

Run from repository root: python3 scripts/split_main_rs.py
"""

from __future__ import annotations

from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAIN = ROOT / "grit-cli" / "src" / "main.rs"
ARGS_MOD = ROOT / "grit-cli" / "src" / "args" / "mod.rs"
ARGS_ARGV = ROOT / "grit-cli" / "src" / "args" / "argv.rs"
DISPATCH = ROOT / "grit-cli" / "src" / "dispatch.rs"
CONTRACT = ROOT / "grit-cli" / "src" / "contract_tests.rs"

# 1-based line numbers inclusive (from current main.rs layout)
ARGS_START = 72
ARGS_END = 2435  # through ServeArgs struct
ARGV_START = 2437
ARGV_END = 2525  # through parse_cli
HELPERS_START = 2664
HELPERS_END = 2885  # through emit_rebase before dispatch
DISPATCH_START = 2887
DISPATCH_END = 4820
TESTS_START = 4828  # inside `mod tests {` — skip opening
TESTS_END = 6004


def line_range(text: str, start: int, end: int) -> str:
    lines = text.splitlines(keepends=True)
    return "".join(lines[start - 1 : end])


def main() -> None:
    text = MAIN.read_text()
    args_body = line_range(text, ARGS_START, ARGS_END)
    argv_body = line_range(text, ARGV_START, ARGV_END)
    helpers_body = line_range(text, HELPERS_START, HELPERS_END)
    dispatch_body = line_range(text, DISPATCH_START, DISPATCH_END)
    tests_body = line_range(text, TESTS_START, TESTS_END)

    ARGS_MOD.parent.mkdir(parents=True, exist_ok=True)

    args_mod = f"""//! Clap argument definitions for `grit` (root [`Cli`], [`Command`], and nested actions).

mod argv;

pub use argv::{{normalize_log_limit_argv, normalize_tag_verify_argv, parse_cli}};

use std::ffi::OsString;

use clap::{{Parser, Subcommand}};
use clap_complete::Shell;

use crate::diff_args;
use crate::global_args;

{args_body}
"""
    ARGS_MOD.write_text(args_mod)

    argv_mod = f"""//! argv normalizers and [`super::Cli`] parsing.

use std::ffi::OsString;

use clap::{{CommandFactory, FromArgMatches, Parser}};

use crate::cli_command;
use crate::diff_args;

use super::Cli;

{argv_body.replace("fn parse_cli", "pub fn parse_cli").replace("fn normalize_log_limit_argv", "pub fn normalize_log_limit_argv").replace("fn normalize_tag_verify_argv", "pub fn normalize_tag_verify_argv")}
"""
    ARGS_ARGV.write_text(argv_mod)

    dispatch_mod = f"""//! Dispatch parsed [`crate::args::Command`] values to command implementations.

use anyhow::{{bail, Result}};
use grit_lib::command_runner::hooks::HookStdio;

use crate::args::{{Command, HistoryAction, RemoteAction, ShareAction, SliceAction, StashAction, ServeAction, AuthAction, BundleAction, GcAction, HooksAction, IgnoreAction, NotesAction, ReflogAction, WorktreeAction}};
use crate::context;
use crate::exit_status::ExitStatus;
use crate::output::{{self, OutputMode, OutputOptions, RenderContext}};
use crate::commands;

/// Bundled per-invocation CLI context passed into each command's `dispatch`.
pub struct CommandEnv<'a> {{
    pub opts: &'a OutputOptions,
    pub ctx: &'a RenderContext,
    pub process_env: &'a grit_lib::config::environment::Environment,
}}

{helpers_body.replace("fn writes_objects", "pub(crate) fn writes_objects").replace("fn talks_to_remotes", "pub(crate) fn talks_to_remotes").replace("fn history_action_shared", "fn history_action_shared").replace("fn validate_cli", "pub fn validate_cli").replace("fn emit_success", "fn emit_success").replace("fn emit_rebase", "pub(crate) fn emit_rebase")}

pub fn dispatch(cli: crate::args::Cli, env: CommandEnv<'_>) -> Result<ExitStatus> {{
    {dispatch_body.replace("fn dispatch(\n    cli: Cli,\n    opts: &OutputOptions,\n    ctx: &RenderContext,\n    process_env: &grit_lib::config::environment::Environment,\n) -> Result<ExitStatus> {", "").rstrip()[:-1]}
}}
"""
    # Fix dispatch - the replacement might be messy; write dispatch body more carefully
    DISPATCH.write_text(dispatch_mod)

    contract = f"""//! CLI contract tests (docs, snapshots, argv normalizers).

#[cfg(test)]
mod tests {{
{tests_body}
}}
"""
    CONTRACT.write_text(contract)
    print("Wrote", ARGS_MOD, ARGS_ARGV, DISPATCH, CONTRACT)
    print("Now manually rewrite main.rs and fix compile errors.")


if __name__ == "__main__":
    main()
