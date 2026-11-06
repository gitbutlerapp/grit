#!/usr/bin/env python3
"""One-off mechanical migration: dyn Cancel / NeverCancel -> CancelToken."""

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
)

REPLACEMENTS: list[tuple[str, str]] = [
    ("&dyn crate::progress::Cancel", "&crate::progress::CancelToken"),
    ("&'a dyn crate::progress::Cancel", "&'a crate::progress::CancelToken"),
    ("&dyn grit_lib::progress::Cancel", "&grit_lib::progress::CancelToken"),
    ("&'a dyn Cancel", "&'a CancelToken"),
    ("&dyn Cancel", "&CancelToken"),
    ("Option<&dyn Cancel>", "Option<&CancelToken>"),
    ("Option<&dyn crate::progress::Cancel>", "Option<&crate::progress::CancelToken>"),
    ("Option<&dyn grit_lib::progress::Cancel>", "Option<&grit_lib::progress::CancelToken>"),
    (
        "Arc<dyn Cancel + Send + Sync>",
        "Arc<grit_lib::progress::CancelToken>",
    ),
    (
        "Arc<dyn grit_lib::progress::Cancel + Send + Sync>",
        "Arc<grit_lib::progress::CancelToken>",
    ),
    ("&crate::progress::NeverCancel", "&crate::progress::CancelToken::never()"),
    ("&grit_lib::progress::NeverCancel", "&grit_lib::progress::CancelToken::never()"),
    ("&NeverCancel", "&CancelToken::never()"),
]

USE_CANCEL_RE = re.compile(
    r"^(\s*)use ([^;]*\b)(Cancel|NeverCancel)([^;]*);",
    re.MULTILINE,
)


def clean_use_cancel(text: str) -> str:
    def repl(m: re.Match[str]) -> str:
        prefix, before, _name, after = m.group(1, 2, 3, 4)
        combined = (before + after).strip()
        combined = re.sub(r",\s*,", ",", combined)
        combined = combined.strip(",").strip()
        if not combined or combined in ("grit_lib::progress", "crate::progress"):
            return f"{prefix}// cancel import removed by migrate_cancel_token"
        return f"{prefix}use {combined};"

    return USE_CANCEL_RE.sub(repl, text)


def process(path: Path) -> bool:
    text = path.read_text(encoding="utf-8")
    orig = text
    for old, new in REPLACEMENTS:
        text = text.replace(old, new)
    if path.suffix == ".rs":
        text = clean_use_cancel(text)
    if text != orig:
        path.write_text(text, encoding="utf-8")
        return True
    return False


def main() -> None:
    changed = 0
    for crate in CRATES:
        base = ROOT / crate
        if not base.is_dir():
            continue
        for path in base.rglob("*"):
            if path.suffix not in (".rs", ".md"):
                continue
            if process(path):
                changed += 1
    print(f"updated {changed} files")


if __name__ == "__main__":
    main()
