#!/usr/bin/env python3
"""Generate Rabin T/U tables for pack delta encoding from upstream diff-delta.c."""

from __future__ import annotations

import argparse
import re
from pathlib import Path


def extract_table(source: str, name: str) -> list[int]:
    pattern = rf"static const unsigned int {name}\[256\] = \{{([^}}]+)\}};"
    match = re.search(pattern, source, re.S)
    if not match:
        raise SystemExit(f"could not find {name}[256] in source")
    return [int(x.strip(), 0) for x in match.group(1).split(",") if x.strip()]


def render_tables(t: list[int], u: list[int]) -> str:
    lines = ["// Rabin tables (format constants from Git diff-delta).\n"]
    for name, vals in [("T", t), ("U", u)]:
        lines.append(f"pub(crate) const {name}: [u32; 256] = [\n")
        for i in range(0, 256, 8):
            chunk = ", ".join(f"0x{v:08x}" for v in vals[i : i + 8])
            lines.append(f"    {chunk},\n")
        lines.append("];\n\n")
    return "".join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--input",
        type=Path,
        default=Path("/tmp/git-scratch/diff-delta.c"),
        help="Path to diff-delta.c (default: %(default)s)",
    )
    parser.add_argument(
        "--output",
        type=Path,
        default=Path("grit-lib/src/roll_hash_tables.rs"),
        help="Rust output path (default: %(default)s)",
    )
    args = parser.parse_args()
    text = args.input.read_text()
    out = render_tables(extract_table(text, "T"), extract_table(text, "U"))
    args.output.write_text(out)
    print(f"wrote {args.output}")


if __name__ == "__main__":
    main()
