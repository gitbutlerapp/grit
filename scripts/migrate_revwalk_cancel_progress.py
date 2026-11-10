#!/usr/bin/env python3
"""Append NullProgress + CancelToken to revwalk/describe/blame entry-point calls only."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CRATES = (
    "grit-lib",
    "grit-cli",
    "grit-protocol",
    "grit-http-server",
    "grit-examples",
    "grit-utils",
    "grit-test-support",
)

# Method names that gained progress+cancel (must be exact identifiers, not `.count` on iterators).
METHOD_PATTERNS = [
    re.compile(r"\.walk\s*\("),
    re.compile(r"\.commits\s*\("),
    re.compile(r"\.rev_list_result\s*\("),
    re.compile(r"\.commits_stream\s*\("),
    re.compile(r"\.commits_iter\s*\("),
    re.compile(r"\.left_right_counts\s*\("),
    re.compile(r"\.rev_list\s*\("),
    re.compile(r"\.describe\s*\("),
    re.compile(r"\.blame\s*\("),
    re.compile(r"\.count\s*\(\s*&"),  # RevWalk::count(&tokens, ...
]
REV_LIST_FN = re.compile(r"(?<![:.])\brev_list\s*\(")
OPEN_COMMIT = re.compile(r"\bopen_commit_stream\s*\(")

SUFFIX = ", &mut NullProgress, &CancelToken::never()"
SUFFIX_REF = ", &mut NullProgress, CancelToken::never_ref()"
CRATE_SUFFIX_REF = ", &mut crate::progress::NullProgress, crate::progress::CancelToken::never_ref()"


def find_call_end(text: str, open_paren: int) -> int | None:
    depth = 0
    i = open_paren
    in_string = False
    escape = False
    while i < len(text):
        ch = text[i]
        if in_string:
            if escape:
                escape = False
            elif ch == "\\":
                escape = True
            elif ch == '"':
                in_string = False
            i += 1
            continue
        if ch == '"':
            in_string = True
            i += 1
            continue
        if ch == "(":
            depth += 1
        elif ch == ")":
            depth -= 1
            if depth == 0:
                return i
        i += 1
    return None


def already_migrated(close: int, text: str) -> bool:
    window = text[max(0, close - 140) : close + 3]
    return "NullProgress" in window


def patch_patterns(text: str, patterns: list[re.Pattern[str]], suffix: str) -> str:
    for pattern in patterns:
        out: list[str] = []
        last = 0
        for m in pattern.finditer(text):
            out.append(text[last : m.start()])
            open_paren = m.end() - 1
            close = find_call_end(text, open_paren)
            if close is None:
                out.append(text[m.start() :])
                return "".join(out)
            if already_migrated(close, text):
                out.append(text[m.start() : close + 1])
            else:
                out.append(text[m.start():close] + suffix + ")")
            last = close + 1
        out.append(text[last:])
        text = "".join(out)
    return text


def ensure_use_grit_lib(text: str) -> str:
    if "NullProgress" not in text or "use grit_lib::progress" in text:
        return text
    if text.startswith("#!"):
        lines = text.splitlines(keepends=True)
        for i, line in enumerate(lines):
            if line.startswith("use ") and "grit_lib" in line:
                return text
        for i, line in enumerate(lines):
            if line.startswith("#![allow") or line.startswith("use grit_lib"):
                lines.insert(i, "use grit_lib::progress::{CancelToken, NullProgress};\n")
                return "".join(lines)
        lines.insert(0, "use grit_lib::progress::{CancelToken, NullProgress};\n")
        return "".join(lines)
    return f"use grit_lib::progress::{{CancelToken, NullProgress}};\n\n{text}"


def ensure_use_crate_progress(text: str) -> str:
    if "crate::progress::NullProgress" in text:
        return text
    if CRATE_SUFFIX_REF not in text:
        return text
    if "use crate::progress" in text:
        return text
    lines = text.splitlines(keepends=True)
    for i, line in enumerate(lines):
        if line.startswith("use "):
            lines.insert(i + 1, "use crate::progress::{CancelToken, NullProgress};\n")
            return "".join(lines)
    return f"use crate::progress::{{CancelToken, NullProgress}};\n{text}"


def process_file(path: Path) -> bool:
    rel = path.relative_to(ROOT)
    text = path.read_text(encoding="utf-8")
    orig = text
    is_grit_lib_src = rel.parts[0] == "grit-lib" and "src" in rel.parts
    suffix = SUFFIX_REF if is_grit_lib_src else SUFFIX
    crate_suffix = CRATE_SUFFIX_REF

    text = patch_patterns(text, METHOD_PATTERNS, suffix)
    if is_grit_lib_src:
        text = patch_patterns(text, [REV_LIST_FN, OPEN_COMMIT], crate_suffix)
    if not is_grit_lib_src:
        text = ensure_use_grit_lib(text)
    else:
        text = ensure_use_crate_progress(text)

    if text != orig:
        path.write_text(text, encoding="utf-8")
        return True
    return False


def main() -> None:
    changed = 0
    skip_under = {"target", "site", "docs"}
    for crate in CRATES:
        base = ROOT / crate
        if not base.is_dir():
            continue
        for path in base.rglob("*.rs"):
            if any(p in skip_under for p in path.parts):
                continue
            if process_file(path):
                changed += 1
    print(f"updated {changed} files")


if __name__ == "__main__":
    main()
