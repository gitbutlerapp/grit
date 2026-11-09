#!/usr/bin/env python3
"""One-shot helper: extract outcome+render block from a command file (for manual review)."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "grit-cli" / "src"


def extract_render_block(command_rs: str) -> str | None:
    if "impl HumanRender" not in command_rs:
        return None
    start = None
    for i, line in enumerate(command_rs.splitlines()):
        if "impl HumanRender" in line:
            # Walk back over outcome types
            j = i
            while j > 0:
                prev = command_rs.splitlines()[j - 1].strip()
                if prev.startswith("use ") or prev.startswith("pub use "):
                    break
                if prev.startswith("///") or prev.startswith("#[") or prev.startswith("pub ") or prev.startswith("fn ") or prev == "}" or prev.startswith("impl ") and "HumanRender" not in prev:
                    j -= 1
                    continue
                if not prev:
                    j -= 1
                    continue
                break
            start = j
            break
    if start is None:
        return None
    lines = command_rs.splitlines()
    depth = 0
    end = None
    for k in range(i, len(lines)):
        depth += lines[k].count("{") - lines[k].count("}")
        if k > i and depth == 0 and lines[k].strip() == "}":
            end = k + 1
            break
    if end is None:
        return None
    return "\n".join(lines[start:end]) + "\n"


def main() -> None:
    cmd = sys.argv[1]
    path = ROOT / "commands" / f"{cmd}.rs"
    if cmd == "diff":
        path = ROOT / "commands" / "diff" / "mod.rs"
    text = path.read_text()
    block = extract_render_block(text)
    if not block:
        print(f"No render block in {path}", file=sys.stderr)
        sys.exit(1)
    print(block)


if __name__ == "__main__":
    main()
