#!/usr/bin/env python3
"""One-off mechanical migration: dyn Cancel / NeverCancel -> CancelToken."""

from __future__ import annotations

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
    ("&mut grit_lib::progress::NeverCancel", "&grit_lib::progress::CancelToken::never()"),
    ("&crate::progress::NeverCancel", "&crate::progress::CancelToken::never()"),
    ("&grit_lib::progress::NeverCancel", "&grit_lib::progress::CancelToken::never()"),
    ("&NeverCancel", "&CancelToken::never()"),
    ("use grit_lib::progress::{NeverCancel, NullProgress};", "use grit_lib::progress::{CancelToken, NullProgress};"),
    ("use grit_lib::progress::NeverCancel;", "use grit_lib::progress::CancelToken;"),
    ("use crate::progress::{NeverCancel, NullProgress};", "use crate::progress::{CancelToken, NullProgress};"),
    ("grit_lib::progress::NeverCancel", "grit_lib::progress::CancelToken"),
]

def process(path: Path) -> bool:
    text = path.read_text(encoding="utf-8")
    orig = text
    for old, new in REPLACEMENTS:
        text = text.replace(old, new)
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
