#!/usr/bin/env python3
"""Mechanical per-command split for grit-cli args + dispatch (task #7447).

Run from repo root:
  python3 scripts/split_per_command_7447.py args
  python3 scripts/split_per_command_7447.py dispatch
"""

from __future__ import annotations

import argparse
import re
import textwrap
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
ARGS_MOD = ROOT / "grit-cli" / "src" / "args" / "mod.rs"
ARGS_MOD_BAK = ROOT / "grit-cli" / "src" / "args" / "mod.rs.bak"
DISPATCH_RS = ROOT / "grit-cli" / "src" / "dispatch.rs"
DISPATCH_BAK = ROOT / "grit-cli" / "src" / "dispatch.rs.bak"
ARGS_DIR = ROOT / "grit-cli" / "src" / "args"
COMMANDS_DIR = ROOT / "grit-cli" / "src" / "commands"
DISPATCH_DIR = ROOT / "grit-cli" / "src" / "dispatch"

VARIANT_MODULE: dict[str, str] = {
    "MergeBase": "merge_base_cmd",
    "PatchId": "patch_id",
    "Integrate": "checkin",
    "Rebase": "history",
    "UploadPack": "serve",
    "ReceivePack": "serve",
}

DISPATCH_FN: dict[str, str] = {
    "Integrate": "dispatch_integrate",
    "Rebase": "dispatch_rebase",
    "UploadPack": "dispatch_upload_pack",
    "ReceivePack": "dispatch_receive_pack",
}

REUSE_ARGS_TYPE: dict[str, str] = {
    "Status": "crate::commands::status::StatusCliArgs",
    "Shortlog": "crate::commands::shortlog::ShortlogCliArgs",
    "Describe": "crate::commands::describe::DescribeCliArgs",
    "Fetch": "crate::commands::fetch_args::FetchArgs",
}

FIELD_ALIAS: dict[str, str] = {
    "Status": "status",
    "Shortlog": "shortlog",
    "Describe": "describe",
    "Fetch": "fetch",
}

UNIT_VARIANTS = frozenset({"Sync", "Update", "Skill"})


def pascal_to_snake(name: str) -> str:
    return re.sub(r"(?<!^)(?=[A-Z])", "_", name).lower()


def args_type(name: str) -> str:
    return f"{name}Args"


def find_matching_brace(text: str, open_idx: int) -> int:
    depth = 0
    for i in range(open_idx, len(text)):
        if text[i] == "{":
            depth += 1
        elif text[i] == "}":
            depth -= 1
            if depth == 0:
                return i
    raise RuntimeError(f"unclosed brace at {open_idx}")


def parse_command_variants(text: str) -> tuple[str, list[tuple[str, str, str]], str]:
    marker = "pub(crate) enum Command {"
    start = text.index(marker)
    brace_open = text.index("{", start)
    brace_close = find_matching_brace(text, brace_open)
    before = text[:start]
    body = text[brace_open + 1 : brace_close]
    after = text[brace_close + 1 :]

    variants: list[tuple[str, str, str]] = []
    i = 0
    while i < len(body):
        doc_lines: list[str] = []
        while True:
            rest = body[i:].lstrip()
            if not rest:
                i = len(body)
                break
            if rest.startswith("///"):
                line_end = body.find("\n", i)
                if line_end == -1:
                    line_end = len(body)
                doc_lines.append(body[i:line_end].rstrip())
                i = line_end + 1 if line_end < len(body) else len(body)
                continue
            if rest.startswith("#["):
                attr_m = re.match(r"\s*(#\[.*?\])\s*", body[i:], re.DOTALL)
                if not attr_m:
                    break
                doc_lines.append(attr_m.group(1).strip())
                i += attr_m.end()
                continue
            break
        doc = "\n".join(doc_lines)
        if i >= len(body):
            break
        name_m = re.match(r"\s*([A-Z][A-Za-z0-9]*)\s*(\{)?", body[i:])
        if not name_m:
            break
        name = name_m.group(1)
        i += name_m.end()
        if name_m.group(2):
            brace_open_v = i - 1
            brace_close_v = find_matching_brace(body, brace_open_v)
            fields = body[brace_open_v + 1 : brace_close_v].strip()
            i = brace_close_v + 1
        else:
            fields = ""
        while i < len(body) and body[i] in " \t\n\r,":
            i += 1
        variants.append((name, doc, fields))
    return before, variants, after


def write_args_variant(name: str, doc: str, fields: str) -> None:
    snake = pascal_to_snake(name)
    path = ARGS_DIR / f"{snake}.rs"

    if name in REUSE_ARGS_TYPE:
        path.write_text(
            f"//! Clap arguments for `grit {snake.replace('_', '-')}`.\n\n"
            f"pub(crate) type {args_type(name)} = {REUSE_ARGS_TYPE[name]};\n"
        )
        return

    if name in UNIT_VARIANTS:
        path.write_text(
            f"//! Clap arguments for `grit {snake.replace('_', '-')}`.\n\n"
            f"#[derive(Debug, Default, clap::Args)]\n"
            f"pub(crate) struct {args_type(name)} {{}}\n"
        )
        return

    if name == "Serve":
        struct_name = "ServeCommandArgs"
    else:
        struct_name = args_type(name)

    if name in ("UploadPack", "ReceivePack"):
        path.write_text(
            f"//! Clap arguments for `grit {snake.replace('_', '-')}`.\n\n"
            f"#[derive(Debug, clap::Args)]\n"
            f"pub(crate) struct {args_type(name)} {{\n"
            f"    #[command(flatten)]\n"
            f"    pub(crate) inner: super::ServeArgs,\n"
            f"}}\n"
        )
        return

    extra = []
    if "Shell" in fields:
        extra.append("use clap_complete::Shell;")
    if "parse_init_object_format" in fields:
        extra.append("use super::parse_init_object_format;")

    doc_block = f"{doc}\n" if doc else ""
    path.write_text(
        f"//! Clap arguments for `grit {snake.replace('_', '-')}`.\n\n"
        f"use clap::Args;\n"
        + ("\n".join(extra) + "\n\n" if extra else "\n")
        + f"{doc_block}#[derive(Debug, Args)]\n"
        f"pub(crate) struct {struct_name} {{\n"
        f"{fields}\n"
        f"}}\n"
    )


def exported_args_type(name: str) -> str:
    if name == "Serve":
        return "ServeCommandArgs"
    return args_type(name)


def build_command_enum(variants: list[tuple[str, str, str]]) -> str:
    lines = [
        "/// Top-level `grit` commands.",
        "///",
        "/// In the binary, each subcommand's arguments are built only when that subcommand is parsed,",
        "/// so startup does not pay for the whole tree. Help output and completions go through",
        "/// [`cli_command::full_cli_command`] so deferred building cannot replace a subcommand's",
        "/// description. Unit tests keep the tree eager: the contract, docs and schema checks walk each",
        "/// command's declared arguments, which `Command::build` would mix with globals inherited from",
        "/// the parent.",
        "#[derive(Debug, Subcommand)]",
        "#[cfg_attr(not(test), command(defer = true))]",
        "pub(crate) enum Command {",
    ]
    for name, doc, _ in variants:
        if doc:
            lines.append(doc)
        lines.append(f"    {name} {{")
        lines.append("        #[command(flatten)]")
        lines.append(f"        args: {exported_args_type(name)},")
        lines.append("    },")
    lines.append("}")
    return "\n".join(lines) + "\n"


def split_args() -> list[tuple[str, str, str]]:
    source = ARGS_MOD_BAK.read_text() if ARGS_MOD_BAK.exists() else ARGS_MOD.read_text()
    before, variants, after = parse_command_variants(source)

    ARGS_DIR.mkdir(parents=True, exist_ok=True)
    for name, doc, fields in variants:
        write_args_variant(name, doc, fields)

    # Insert per-command `mod` + `pub use` after argv re-export block.
    marker = "pub use argv::{normalize_log_limit_argv, normalize_tag_verify_argv, parse_cli};"
    insert_at = before.index(marker) + len(marker)
    head = before[:insert_at]
    tail_enums = before[insert_at:]

    mod_decls = ["", ""]
    for name, _, _ in variants:
        mod_decls.append(f"mod {pascal_to_snake(name)};")
    mod_decls.append("")
    for name, _, _ in variants:
        snake = pascal_to_snake(name)
        exported = exported_args_type(name)
        if name == "Serve":
            mod_decls.append(f"pub(crate) use {snake}::{exported};")
        else:
            mod_decls.append(f"pub(crate) use {snake}::{exported};")
    mod_decls.append("")

    prelude_marker = "/// Top-level `grit` commands."
    if prelude_marker in tail_enums:
        tail_enums = tail_enums[: tail_enums.index(prelude_marker)].rstrip() + "\n\n"
    new_mod = head + "\n".join(mod_decls) + tail_enums + build_command_enum(variants) + after
    ARGS_MOD.write_text(new_mod)
    if not ARGS_MOD_BAK.exists():
        ARGS_MOD_BAK.write_text(source)
    print(f"Wrote args/mod.rs + {len(variants)} args/*.rs")
    return variants


def field_names(fields: str) -> list[str]:
    out: list[str] = []
    for line in fields.splitlines():
        m = re.match(r"^\s*(\w+)\s*:", line)
        if m:
            out.append(m.group(1))
    return out


def strip_arm_header(arm: str, variant: str) -> str:
    """Turn one or more `Command::Variant` match arms into a straight-line body."""
    chunks: list[str] = []
    rest = arm.strip()
    guard_re = rf"Command::{re.escape(variant)}\s*\{{[^}}]*\}}(\s+if[^\n]+)?\s*=>\s*"
    plain_re = rf"Command::{re.escape(variant)}(\s+if[^\n]+)?\s*=>\s*"
    while rest:
        m = re.search(guard_re, rest, flags=re.DOTALL) or re.search(plain_re, rest, flags=re.DOTALL)
        if not m:
            break
        rest = rest[m.end() :]
        # Take until next Command:: at line start or end.
        nxt = re.search(r"\n\s*Command::", rest)
        chunk = rest[: nxt.start()] if nxt else rest
        guard = m.group(1) if m.re.pattern == guard_re and m.lastindex else ""
        chunk = chunk.strip()
        if guard:
            chunks.append(f"if {guard.strip()} {{\n        {chunk}\n    }} else ")
        else:
            chunks.append(chunk)
        rest = rest[nxt.start() :] if nxt else ""
    if not chunks:
        return arm.strip()
    body = "".join(chunks)
    if body.endswith(" else "):
        body = body[: -len(" else ")]
    return body.strip()


def clean_arm_tail(body: str) -> str:
    body = body.rstrip()
    if body.endswith(","):
        body = body[:-1].rstrip()
    return body


def bind_args_prelude(variant: str, fields: str) -> str:
    if variant in FIELD_ALIAS:
        alias = FIELD_ALIAS[variant]
        return f"    let {alias} = args;"
    names = field_names(fields)
    if not names:
        return ""
    if len(names) == 1 and names[0] == "args" and variant in ("UploadPack", "ReceivePack"):
        return "    let args = args.inner;"
    return f"    let {args_type(variant)} {{ {', '.join(names)} }} = args;"


def extract_dispatch_arm_groups(dispatch_text: str) -> dict[str, str]:
    match_start = dispatch_text.index("match cli.command")
    unwrap_close = dispatch_text.index("}) {", match_start) + 2
    open_b = dispatch_text.index("{", unwrap_close)
    close_b = find_matching_brace(dispatch_text, open_b)
    inner = dispatch_text[open_b + 1 : close_b]

    groups: dict[str, list[str]] = {}
    current: str | None = None
    buf: list[str] = []
    for line in inner.splitlines(keepends=True):
        m = re.match(r"\s*Command::(\w+)", line)
        if m:
            if current:
                groups.setdefault(current, []).append("".join(buf))
            current = m.group(1)
            buf = [line]
        else:
            buf.append(line)
    if current:
        groups.setdefault(current, []).append("".join(buf))

    def trim_inter_arm_comments(blob: str) -> str:
        lines = blob.splitlines()
        while lines and lines[-1].strip().startswith("//"):
            lines.pop()
        return "\n".join(lines)

    return {k: trim_inter_arm_comments("".join(v)) for k, v in groups.items()}


def write_support(dispatch_text: str) -> None:
    DISPATCH_DIR.mkdir(parents=True, exist_ok=True)
    start = dispatch_text.index("/// Commands that write objects")
    end = dispatch_text.index("pub fn dispatch(")
    helpers = dispatch_text[start:end]
    header = """//! Shared dispatch helpers.

use anyhow::{bail, Result};

use crate::args::{Cli, Command, HistoryAction, StashAction};
use crate::commands;
use crate::exit_status::ExitStatus;
use crate::output::{self, emit, OutputMode, OutputOptions, RenderContext};
use crate::json_error;
use crate::quiet_args;

/// Invocation context shared by every command `dispatch`.
pub struct CommandEnv<'a> {
    pub opts: &'a OutputOptions,
    pub ctx: &'a RenderContext,
    pub process_env: &'a grit_lib::config::environment::Environment,
}

"""
    helpers = helpers.replace(
        "Command::Commit { dry_run, .. } => !dry_run,",
        "Command::Commit { args, .. } => !args.dry_run,",
    )
    helpers = helpers.replace(
        "Command::Merge { check, dry_run, .. }\n        | Command::Pick { check, dry_run, .. }\n        | Command::Revert { check, dry_run, .. } => !check && !dry_run,",
        "Command::Merge { args, .. }\n        | Command::Pick { args, .. }\n        | Command::Revert { args, .. } if !args.check && !args.dry_run => true,",
    )
    helpers = helpers.replace("Command::Fetch { fetch } => !fetch.dry_run,", "Command::Fetch { args } => !args.dry_run,")
    helpers = helpers.replace(
        "Command::Pull { fetch, check, .. } => !fetch.dry_run && !check,",
        "Command::Pull { args, .. } => !args.fetch.dry_run && !args.check,",
    )
    helpers = helpers.replace(
        "Command::Rebase { shared, .. } => !shared.dry_run,",
        "Command::Rebase { args, .. } => !args.shared.dry_run,",
    )
    helpers = helpers.replace(
        "Command::History { action } => !history_action_shared(action).dry_run,",
        "Command::History { args } => !history_action_shared(&args.action).dry_run,",
    )
    helpers = helpers.replace(
        "Command::Stash {\n            dry_run, action, ..\n        } => {",
        "Command::Stash { args, .. } => {",
    )
    helpers = helpers.replace(
        "!dry_run\n                && matches!(\n                    action,",
        "!args.dry_run\n                && matches!(\n                    args.action,",
    )
    helpers = helpers.replace(
        "if let Some(Command::Blame { copy_detection, .. }) = &cli.command {",
        "if let Some(Command::Blame { args, .. }) = &cli.command {",
    )
    helpers = helpers.replace("if *copy_detection > 3", "if args.copy_detection > 3")

    priv_helpers = dispatch_text[
        dispatch_text.index("fn is_broken_pipe"): dispatch_text.index("pub fn dispatch(")
    ]
    for fn in (
        "emit_success_quiet",
        "emit_rebase_outcome",
        "finish_hunk_error",
        "emit_success",
        "is_broken_pipe",
    ):
        priv_helpers = priv_helpers.replace(f"fn {fn}", f"pub(crate) fn {fn}")

    (DISPATCH_DIR / "support.rs").write_text(header + helpers + priv_helpers)


def append_dispatch(variant: str, fields: str, arm: str) -> None:
    mod = VARIANT_MODULE.get(variant, pascal_to_snake(variant))
    path = COMMANDS_DIR / f"{mod}.rs"
    if not path.exists():
        print(f"WARN: missing {path} for {variant}")
        return
    text = path.read_text()
    fn_name = DISPATCH_FN.get(variant, "dispatch")
    if f"pub fn {fn_name}(" in text:
        return

    body = clean_arm_tail(strip_arm_header(arm, variant))
    prelude = bind_args_prelude(variant, fields)
    args_ty = exported_args_type(variant) if variant == "Serve" else args_type(variant)
    prelude_line = prelude or ""
    fn_block = f"""

/// Dispatch parsed CLI args for `{variant}`.
pub fn {fn_name}(args: {args_ty}, env: crate::dispatch::support::CommandEnv<'_>) -> anyhow::Result<crate::exit_status::ExitStatus> {{
    use anyhow::{{bail, Result}};
    use crate::args::*;
    use crate::commands;
    use crate::context;
    use crate::dispatch::support::{{emit_success, emit_success_quiet, finish_hunk_error, emit_rebase_outcome, CommandEnv}};
    use crate::exit_status::ExitStatus;
    use crate::json_error;
    use crate::message_input;
    use crate::output::{{self, OutputOptions, RenderContext}};
    use crate::pager;
    use crate::patch_selection;
    use crate::progress;
    use crate::quiet_args;
    use crate::stdio;
    let CommandEnv {{ opts, ctx, process_env }} = env;
{prelude_line}
    {body}
}}
"""
    path.write_text(text.rstrip() + fn_block + "\n")


def write_dispatch_mod(variants: list[tuple[str, str, str]]) -> None:
    lines = [
        "//! Route a parsed [`crate::args::Command`] to command implementations.",
        "",
        "mod support;",
        "",
        "pub use support::{",
        "    emit_rebase_outcome, reject_non_human_plumbing, talks_to_remotes, validate_cli,",
        "    writes_objects, CommandEnv,",
        "};",
        "",
        "use anyhow::Result;",
        "",
        "use crate::args::{Command, StatusArgs};",
        "use crate::commands;",
        "use crate::exit_status::ExitStatus;",
        "",
        "pub fn dispatch(cli: crate::args::Cli, env: CommandEnv<'_>) -> Result<ExitStatus> {",
        "    match cli.command.unwrap_or(Command::Status {",
        "        args: StatusArgs::default(),",
        "    }) {",
    ]
    for name, _, _ in variants:
        mod = VARIANT_MODULE.get(name, pascal_to_snake(name))
        fn = DISPATCH_FN.get(name, "dispatch")
        lines.append(f"        Command::{name} {{ args }} => commands::{mod}::{fn}(args, env),")
    lines.append("    }")
    lines.append("}")
    (DISPATCH_DIR / "mod.rs").write_text("\n".join(lines) + "\n")
    if DISPATCH_RS.exists():
        DISPATCH_RS.unlink()


def split_dispatch(variants: list[tuple[str, str, str]]) -> None:
    source = DISPATCH_BAK.read_text() if DISPATCH_BAK.exists() else DISPATCH_RS.read_text()
    write_support(source)
    arms = extract_dispatch_arm_groups(source)
    fields_map = {n: f for n, _, f in variants}
    for variant, arm in arms.items():
        append_dispatch(variant, fields_map.get(variant, ""), arm)
    write_dispatch_mod(variants)
    print(f"Wrote dispatch/ + {len(arms)} command dispatch fns")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("phase", choices=["args", "dispatch", "all"], nargs="?", default="all")
    args = ap.parse_args()
    if args.phase in ("args", "all"):
        variants = split_args()
    else:
        _, variants, _ = parse_command_variants(ARGS_MOD.read_text())
    if args.phase in ("dispatch", "all"):
        split_dispatch(variants)


if __name__ == "__main__":
    main()
