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

IDENT_CONTINUE = frozenset("abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789_")


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


def _char_literal_end(text: str, start: int) -> int | None:
    """If text[start] is `'`, return index after a char literal or None."""
    if start >= len(text) or text[start] != "'":
        return None
    j = start + 1
    if j >= len(text):
        return None
    if text[j] == "\\":
        j += 1
        if j >= len(text):
            return None
        j += 1
    elif text[j] == "'":
        return None
    else:
        j += 1
    if j < len(text) and text[j] == "'":
        return j + 1
    return None


def _quoted_string_end(text: str, quote_start: int) -> int | None:
    """Return index after a "..." string starting at quote_start, or None."""
    if quote_start >= len(text) or text[quote_start] != '"':
        return None
    i = quote_start + 1
    n = len(text)
    while i < n:
        ch = text[i]
        if ch == "\\":
            i += 2
            continue
        if ch == '"':
            return i + 1
        i += 1
    return None


def _raw_string_end(text: str, start: int) -> int | None:
    """Return index after r#*"..."#* or br#*"..."#* starting at start."""
    i = start
    n = len(text)
    if i < n and text[i] == "b":
        i += 1
    if i + 1 >= n or text[i] != "r":
        return None
    i += 1
    hash_count = 0
    while i < n and text[i] == "#":
        hash_count += 1
        i += 1
    if i >= n or text[i] != '"':
        return None
    i += 1
    closing = f'"{("#" * hash_count)}'
    end = text.find(closing, i)
    if end == -1:
        return None
    return end + len(closing)


def _byte_or_plain_string_end(text: str, start: int) -> int | None:
    """Return index after b"..." or "..." starting at start."""
    i = start
    if i < len(text) and text[i] == "b":
        i += 1
    if i >= len(text) or text[i] != '"':
        return None
    return _quoted_string_end(text, i)


def scrub_rust_literals(text: str) -> str:
    """Replace string/char literals with spaces; keep lifetimes, labels, and comments intact."""
    out: list[str] = []
    i = 0
    n = len(text)
    in_line_comment = False
    block_comment_depth = 0

    while i < n:
        if in_line_comment:
            out.append(text[i])
            if text[i] == "\n":
                in_line_comment = False
            i += 1
            continue

        if block_comment_depth > 0:
            if i + 1 < n and text[i : i + 2] == "/*":
                out.append("/*")
                block_comment_depth += 1
                i += 2
                continue
            if i + 1 < n and text[i : i + 2] == "*/":
                out.append("*/")
                block_comment_depth -= 1
                i += 2
                continue
            out.append(text[i])
            i += 1
            continue

        if i + 1 < n and text[i : i + 2] == "//":
            out.append("//")
            in_line_comment = True
            i += 2
            continue
        if i + 1 < n and text[i : i + 2] == "/*":
            out.append("/*")
            block_comment_depth = 1
            i += 2
            continue

        raw_end = _raw_string_end(text, i)
        if raw_end is not None:
            out.append(" " * (raw_end - i))
            i = raw_end
            continue
        if text[i] == "b" and i + 1 < n and text[i + 1] == "'":
            end = _char_literal_end(text, i + 1)
            if end is not None:
                out.append(" " * (end - i))
                i = end
                continue
        if text[i] == "b":
            str_end = _byte_or_plain_string_end(text, i)
            if str_end is not None and str_end > i + 1:
                out.append(" " * (str_end - i))
                i = str_end
                continue
        if text[i] == '"':
            str_end = _quoted_string_end(text, i)
            if str_end is not None:
                out.append(" " * (str_end - i))
                i = str_end
                continue
        if text[i] == "'":
            char_end = _char_literal_end(text, i)
            if char_end is not None:
                out.append(" " * (char_end - i))
                i = char_end
                continue
            j = i + 1
            if j < n and (text[j].isalpha() or text[j] == "_"):
                while j < n and text[j] in IDENT_CONTINUE:
                    j += 1
                out.append(text[i:j])
                i = j
                continue
        out.append(text[i])
        i += 1
    return "".join(out)


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
    scrubbed = scrub_rust_literals(text)
    lines = text.splitlines()
    scrubbed_lines = scrubbed.splitlines()
    if len(scrubbed_lines) < len(lines):
        scrubbed_lines.extend([""] * (len(lines) - len(scrubbed_lines)))

    for line_no, scrub_line in enumerate(scrubbed_lines, 1):
        orig_line = lines[line_no - 1] if line_no <= len(lines) else ""
        for flat in flat_modules_in_line(scrub_line):
            expected = canonical_grit_lib_prefix(flat, mapping)
            if expected is not None:
                violations.append((rel, line_no, flat, expected, orig_line.strip()))

    for line_no, body in grit_lib_group_bodies(scrubbed):
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
