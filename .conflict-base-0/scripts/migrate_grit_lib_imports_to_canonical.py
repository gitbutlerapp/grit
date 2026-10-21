#!/usr/bin/env python3
"""One-shot mechanical migration: flat grit_lib paths -> canonical domain paths."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAP_FILE = ROOT / "grit-lib" / "canonical-modules.toml"

FLAT_USE = re.compile(r"\bgrit_lib::([a-z_][a-z0-9_]*)")


def parse_map() -> dict[str, str]:
    text = MAP_FILE.read_text(encoding="utf-8")
    in_map = False
    mapping: dict[str, str] = {}
    for line in text.splitlines():
        if line.strip() == "[map]":
            in_map = True
            continue
        if not in_map or not line.strip() or line.startswith("#"):
            continue
        if line.startswith("["):
            break
        m = re.match(r'^([a-z_][a-z0-9_]*) = "([^"]*)"$', line.strip())
        if m:
            mapping[m.group(1)] = m.group(2)
    return mapping


def replace_line(line: str, mapping: dict[str, str]) -> str:
    def repl(m: re.Match[str]) -> str:
        flat = m.group(1)
        canon = mapping.get(flat)
        if canon is None or canon == flat:
            return m.group(0)
        return "grit_lib::" + canon.replace(".", "::")

    return FLAT_USE.sub(repl, line)


def main() -> int:
    mapping = parse_map()
    files = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "*.rs"],
        cwd=ROOT,
        text=True,
    ).splitlines()
    changed = 0
    for rel in files:
        if rel.startswith("grit-lib/src/"):
            continue
        path = ROOT / rel
        if not path.is_file():
            continue
        original = path.read_text(encoding="utf-8")
        updated = "\n".join(replace_line(l, mapping) for l in original.splitlines())
        if original.endswith("\n") and not updated.endswith("\n"):
            updated += "\n"
        if updated != original:
            path.write_text(updated, encoding="utf-8")
            changed += 1
            print(rel)
    print(f"Updated {changed} files", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
