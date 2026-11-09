#!/usr/bin/env python3
"""Extract outcome + HumanRender/MarkdownRender block from a command source file."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1] / "grit-cli" / "src" / "commands"


def find_block(lines: list[str]) -> tuple[int, int] | None:
    hr = next((i for i, l in enumerate(lines) if "impl HumanRender" in l), None)
    if hr is None:
        return None
    start = hr
    while start > 0:
        prev = lines[start - 1].strip()
        if prev.startswith("use ") or prev.startswith("pub use ") or prev == "":
            if prev.startswith("use ") or prev.startswith("pub use "):
                break
            start -= 1
            continue
        if (
            prev.startswith("///")
            or prev.startswith("#[")
            or prev.startswith("pub ")
            or prev.startswith("struct ")
            or prev.startswith("enum ")
            or prev.startswith("fn ")
            or prev == "}"
            or (prev.startswith("impl ") and "HumanRender" not in prev)
        ):
            start -= 1
            continue
        break
    depth = 0
    end = None
    for k in range(hr, len(lines)):
        depth += lines[k].count("{") - lines[k].count("}")
        if k > hr and depth == 0 and lines[k].strip() == "}":
            # Might be end of last MarkdownRender impl; continue until no more impl MarkdownRender
            pass
    # find last impl MarkdownRender block end
    last_md = max(i for i, l in enumerate(lines) if "impl MarkdownRender" in l)
    depth = 0
    for k in range(last_md, len(lines)):
        depth += lines[k].count("{") - lines[k].count("}")
        if depth == 0 and k > last_md:
            end = k + 1
            break
    if end is None:
        return None
    # walk back start to include all types before first impl HumanRender
    while start > 0:
        line = lines[start - 1]
        if line.strip().startswith("#[derive") or line.strip().startswith("pub struct") or line.strip().startswith("pub enum") or line.strip().startswith("///") or line.strip().startswith("#[serde") or line.strip().startswith("struct ") or line.strip() == "}" or line.strip().startswith("fn is_false"):
            start -= 1
        elif line.strip() == "":
            start -= 1
        else:
            break
    return start, end


def main() -> None:
    name = sys.argv[1]
    path = ROOT / f"{name}.rs"
    if name == "diff":
        path = ROOT / "diff" / "mod.rs"
    text = path.read_text()
    lines = text.splitlines()
    span = find_block(lines)
    if not span:
        print("no block", file=sys.stderr)
        sys.exit(1)
    s, e = span
    print("\n".join(lines[s:e]))


if __name__ == "__main__":
    main()
