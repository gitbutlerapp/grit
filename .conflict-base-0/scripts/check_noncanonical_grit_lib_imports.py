#!/usr/bin/env python3
"""Fail if any crate outside grit-lib/src uses a non-canonical grit_lib module path."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
MAP_FILE = ROOT / "grit-lib" / "canonical-modules.toml"

FLAT_USE = re.compile(r"\bgrit_lib::([a-z_][a-z0-9_]*)")
GROUP_START = re.compile(r"\bgrit_lib::\{")


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
        if not m:
            raise ValueError(f"bad map line: {line!r}")
        mapping[m.group(1)] = m.group(2)
    return mapping


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


def canonical_grit_lib_prefix(flat: str, mapping: dict[str, str]) -> str | None:
    canon = mapping.get(flat)
    if canon is None or canon == flat:
        return None
    return "grit_lib::" + canon.replace(".", "::")


def split_top_level_commas(body: str) -> list[str]:
    parts: list[str] = []
    depth = 0
    start = 0
    for i, ch in enumerate(body):
        if ch == "{":
            depth += 1
        elif ch == "}":
            depth -= 1
        elif ch == "," and depth == 0:
            parts.append(body[start:i])
            start = i + 1
    parts.append(body[start:])
    return parts


def first_flat_module_in_use_item(item: str) -> str | None:
    item = item.strip()
    if not item or item.startswith("self") or item.startswith("super") or item.startswith("crate"):
        return None
    m = re.match(r"^([a-z_][a-z0-9_]*)", item)
    if not m:
        return None
    return m.group(1)


def grit_lib_group_bodies(text: str) -> list[tuple[int, str]]:
    """Return (1-based line number of '{', brace body) for each grit_lib::{...} group."""
    groups: list[tuple[int, str]] = []
    for m in GROUP_START.finditer(text):
        open_brace = m.end() - 1
        depth = 0
        i = open_brace
        while i < len(text):
            ch = text[i]
            if ch == "{":
                depth += 1
            elif ch == "}":
                depth -= 1
                if depth == 0:
                    body = text[open_brace + 1 : i]
                    line_no = text.count("\n", 0, m.start()) + 1
                    groups.append((line_no, body))
                    break
            i += 1
    return groups


def flat_modules_in_line(line: str) -> list[str]:
    return [m.group(1) for m in FLAT_USE.finditer(line)]


def flat_modules_in_group_body(body: str) -> list[str]:
    mods: list[str] = []
    for item in split_top_level_commas(body):
        flat = first_flat_module_in_use_item(item)
        if flat is not None:
            mods.append(flat)
    return mods


def find_violations_in_text(
    rel: str, text: str, mapping: dict[str, str]
) -> list[tuple[str, int, str, str, str]]:
    violations: list[tuple[str, int, str, str, str]] = []
    lines = text.splitlines()

    for line_no, line in enumerate(lines, 1):
        for flat in flat_modules_in_line(line):
            expected = canonical_grit_lib_prefix(flat, mapping)
            if expected is not None:
                violations.append((rel, line_no, flat, expected, line.strip()))

    for line_no, body in grit_lib_group_bodies(text):
        snippet = lines[line_no - 1].strip() if line_no <= len(lines) else "use grit_lib::{...}"
        for flat in flat_modules_in_group_body(body):
            expected = canonical_grit_lib_prefix(flat, mapping)
            if expected is not None:
                violations.append((rel, line_no, flat, expected, snippet))
    return violations


def find_violations(mapping: dict[str, str]) -> list[tuple[str, int, str, str, str]]:
    violations: list[tuple[str, int, str, str, str]] = []
    for path in external_rust_files():
        rel = path.relative_to(ROOT).as_posix()
        text = path.read_text(encoding="utf-8", errors="replace")
        violations.extend(find_violations_in_text(rel, text, mapping))
    return violations


def main() -> int:
    mapping = parse_map()
    violations = find_violations(mapping)
    if not violations:
        return 0
    print(
        "Non-canonical grit_lib imports (use domain paths from canonical-modules.toml):",
        file=sys.stderr,
    )
    for rel, line_no, flat, expected, line in violations[:200]:
        print(f"  {rel}:{line_no}: grit_lib::{flat} -> {expected}", file=sys.stderr)
        print(f"    {line[:120]}", file=sys.stderr)
    if len(violations) > 200:
        print(f"  … and {len(violations) - 200} more", file=sys.stderr)
    print(f"Total: {len(violations)}", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
