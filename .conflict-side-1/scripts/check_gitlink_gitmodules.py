#!/usr/bin/env python3
"""Fail when the index contains a gitlink (mode 160000) without a .gitmodules entry."""

from __future__ import annotations

import configparser
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
GITMODULES = ROOT / ".gitmodules"
MODE_GITLINK = "160000"


def indexed_gitlinks() -> list[str]:
    out = subprocess.check_output(
        ["git", "ls-files", "-s"],
        cwd=ROOT,
        text=True,
    )
    paths: list[str] = []
    for line in out.splitlines():
        parts = line.split(maxsplit=3)
        if len(parts) != 4:
            continue
        mode, _oid, _stage, path = parts
        if mode == MODE_GITLINK:
            paths.append(path)
    return sorted(paths)


def gitmodules_paths() -> set[str]:
    if not GITMODULES.is_file():
        return set()
    parser = configparser.ConfigParser()
    parser.read(GITMODULES, encoding="utf-8")
    paths: set[str] = set()
    for section in parser.sections():
        if not section.startswith('submodule "') or not section.endswith('"'):
            continue
        if parser.has_option(section, "path"):
            paths.add(parser.get(section, "path").strip())
    return paths


def main() -> int:
    gitlinks = indexed_gitlinks()
    if not gitlinks:
        return 0
    configured = gitmodules_paths()
    orphan = [p for p in gitlinks if p not in configured]
    if not orphan:
        return 0
    print(
        "Gitlinks in the index must have a matching [submodule] path in .gitmodules "
        "(Cargo git dependencies fail submodule update otherwise):",
        file=sys.stderr,
    )
    for path in orphan:
        print(f"  {path}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
