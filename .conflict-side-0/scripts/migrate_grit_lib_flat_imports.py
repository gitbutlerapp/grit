#!/usr/bin/env python3
"""Rewrite flat grit_lib imports to canonical paths using canonical-modules.toml."""

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


def canonical_prefix(flat: str, mapping: dict[str, str]) -> str | None:
    canon = mapping.get(flat)
    if canon is None or canon == flat:
        return None
    return "grit_lib::" + canon.replace(".", "::")


def external_rust_files() -> list[Path]:
    files = subprocess.check_output(
        ["git", "ls-files", "--cached", "--others", "--exclude-standard", "*.rs"],
        cwd=ROOT,
        text=True,
    ).splitlines()
    out: list[Path] = []
    for rel in files:
        if rel.startswith("grit-lib/src/"):
            continue
        path = ROOT / rel
        if path.is_file():
            out.append(path)
    return out


def migrate_file(path: Path, mapping: dict[str, str]) -> bool:
    text = path.read_text(encoding="utf-8")
    changed = False

    def repl(m: re.Match[str]) -> str:
        nonlocal changed
        flat = m.group(1)
        canon = canonical_prefix(flat, mapping)
        if canon is None:
            return m.group(0)
        changed = True
        return canon + m.group(0)[len(f"grit_lib::{flat}") :]

    new_text = FLAT_USE.sub(repl, text)
    if changed:
        path.write_text(new_text, encoding="utf-8")
    return changed


def main() -> int:
    mapping = parse_map()
    touched = 0
    for path in external_rust_files():
        if migrate_file(path, mapping):
            touched += 1
            print(path.relative_to(ROOT))
    print(f"migrated {touched} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
