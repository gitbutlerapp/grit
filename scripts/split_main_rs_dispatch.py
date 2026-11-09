#!/usr/bin/env python3
"""Mechanical helper: split dispatch match arms from main.rs into command modules.

Run from repo root after backing up. Not idempotent — inspect output before committing.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAIN = ROOT / "grit-cli" / "src" / "main.rs"


def extract_dispatch_body(text: str) -> str:
    start = text.index("fn dispatch(")
    # Find opening brace of function
    brace = text.index("{", start)
    depth = 0
    i = brace
    while i < len(text):
        c = text[i]
        if c == "{":
            depth += 1
        elif c == "}":
            depth -= 1
            if depth == 0:
                return text[brace + 1 : i]
        i += 1
    raise RuntimeError("unclosed dispatch")


def main() -> None:
    text = MAIN.read_text()
    body = extract_dispatch_body(text)
    # match cli.command.unwrap_or(...) { ... }
    inner = body.strip()
    if not inner.startswith("match"):
        print("unexpected dispatch shape", file=sys.stderr)
        sys.exit(1)
    # arms start after first {
    mstart = inner.index("{") + 1
    mend = inner.rindex("}")
    arms_text = inner[mstart:mend]
    # Split on `Command::` at line start (after whitespace)
    parts = re.split(r"\n(?=\s+Command::)", arms_text)
    print(f"found {len(parts)} dispatch segments")
    for p in parts[:5]:
        first = p.strip().split("\n", 1)[0][:80]
        print(" ", first)


if __name__ == "__main__":
    main()
