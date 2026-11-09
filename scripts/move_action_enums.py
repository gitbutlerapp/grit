#!/usr/bin/env python3
"""Move nested *Action enums from args/mod.rs into matching args/<cmd>.rs files."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MOD = ROOT / "grit-cli" / "src" / "args" / "mod.rs"

# Comment marker -> args file (without .rs)
MOVES: list[tuple[str, str]] = [
    ("remote", "remote"),
    ("reflog", "reflog"),
    ("gc", "gc"),
    ("ignore", "ignore"),
    ("stash", "stash"),
    ("history", "history"),
    ("notes", "notes"),
    ("worktree", "worktree"),
    ("bundle", "bundle"),
    ("hooks", "hooks"),
    ("auth", "auth"),
    ("slice", "slice"),
    ("share", "share"),
    ("serve", "serve"),
]

LOG_GROUP = "log"


def extract_block(text: str, start_marker: str) -> tuple[str, str, str]:
    """Return (before, block, after) where block starts at start_marker."""
    idx = text.index(start_marker)
    before = text[:idx]
    rest = text[idx:]
    # include following RemoteRefSortArg after RemoteAction
    if "remote" in start_marker:
        # extract through RemoteRefSortArg enum closing brace
        m = re.search(
            r"(/// Sort key for.*?\n\}\n)",
            rest,
            re.DOTALL,
        )
        if m:
            end = m.end()
            block = rest[:end]
            after = rest[end:]
            return before, block, after
    # generic: find enum starting after marker, brace-match
    enum_start = rest.find("pub(crate) enum")
    if enum_start < 0:
        raise RuntimeError(f"no enum after {start_marker}")
    depth = 0
    i = enum_start
    while i < len(rest):
        if rest[i] == "{":
            depth += 1
        elif rest[i] == "}":
            depth -= 1
            if depth == 0:
                block = rest[: i + 1] + "\n"
                after = rest[i + 1 :]
                return before, block, after
        i += 1
    raise RuntimeError("unclosed enum")


def main() -> None:
    text = MOD.read_text()
    for cmd, file in MOVES:
        marker = f"/// Subcommands of `grit {cmd}`."
        if marker not in text:
            print("skip", cmd, "(marker missing)")
            continue
        before, block, after = extract_block(text, marker)
        text = before + after
        path = ROOT / "grit-cli" / "src" / "args" / f"{file}.rs"
        body = path.read_text()
        if "Subcommand" not in body:
            body = body.replace(
                "use clap::Args;",
                "use clap::{Args, Subcommand};\n\n" + block,
                1,
            )
        else:
            body = body + "\n" + block
        body = body.replace("use super::RemoteAction;", "use super::RemoteAction;\n")
        # fix remote.rs import: use local enum
        if cmd == "remote":
            body = body.replace("use super::RemoteAction;\n", "")
        path.write_text(body)
        print("moved", cmd, "->", path.name, len(block))

    # LogGroupByArg -> log.rs
    if "pub(crate) enum LogGroupByArg" in text:
        m = re.search(
            r"#\[derive\(Clone, Copy, Debug, clap::ValueEnum\)\]\npub\(crate\) enum LogGroupByArg \{[^}]+\}\n",
            text,
        )
        if m:
            block = m.group(0)
            text = text[: m.start()] + text[m.end() :]
            log = ROOT / "grit-cli" / "src" / "args" / "log.rs"
            lb = log.read_text()
            if "LogGroupByArg" not in lb:
                lb = lb.replace(
                    "use clap::Args;",
                    "use clap::{Args, ValueEnum};\n\n" + block,
                    1,
                )
                log.write_text(lb)

    MOD.write_text(text)
    print("mod.rs now", len(text.splitlines()), "lines")


if __name__ == "__main__":
    main()
