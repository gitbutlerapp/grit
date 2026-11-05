#!/usr/bin/env python3
"""List grit-lib public API paths in a scope with no use outside grit-lib/src."""

from __future__ import annotations

import argparse
import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SNAPSHOT = ROOT / "grit-lib" / "public-api.txt"
EXTERNAL = ("grit-cli", "grit-utils", "grit-examples", "grit-protocol", "grit-http-server")
INTEGRATION = ROOT / "grit-lib" / "tests"

ITEM_RE = re.compile(
    r"^pub (?:fn|struct|enum|type|const|trait|mod) (grit_lib::[^(=\s]+)"
)


def rg_any(needle: str, paths: tuple[str, ...]) -> bool:
    for base in paths:
        r = subprocess.run(
            ["rg", "-q", "--fixed-strings", needle, base],
            cwd=ROOT,
            capture_output=True,
        )
        if r.returncode == 0:
            return True
    return False


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument(
        "--prefix",
        action="append",
        default=[],
        help="Limit to paths starting with this prefix (repeatable)",
    )
    args = p.parse_args()
    prefixes = tuple(args.prefix) if args.prefix else ()

    lines = SNAPSHOT.read_text(encoding="utf-8").splitlines()
    items: list[str] = []
    for ln in lines:
        m = ITEM_RE.match(ln)
        if not m:
            continue
        path = m.group(1)
        if prefixes and not path.startswith(prefixes):
            continue
        items.append(path)

    orphan: list[str] = []
    for path in items:
        if rg_any(path, EXTERNAL):
            continue
        if rg_any(path, (str(INTEGRATION.relative_to(ROOT)),)):
            continue
        if rg_any(path, ("grit-lib/benches",)):
            continue
        orphan.append(path)

    for path in orphan:
        print(path)
    print(f"# total candidates: {len(orphan)}", file=__import__("sys").stderr)


if __name__ == "__main__":
    main()
