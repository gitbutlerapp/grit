#!/usr/bin/env python3
"""Restore `pub` on items referenced from grit-lib/tests (integration crate)."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
TEST_TREES = (
    ROOT / "grit-lib" / "tests",
    ROOT / "grit-examples",
    ROOT / "grit-cli" / "tests",
)
SRC = ROOT / "grit-lib" / "src"

PATH_RE = re.compile(r"grit_lib::[a-zA-Z0-9_]+(?:::[a-zA-Z0-9_]+)*")


def collect_test_paths() -> set[str]:
    args = ["rg", "-o", PATH_RE.pattern]
    for tree in TEST_TREES:
        args.append(str(tree))
    args.extend(["-g", "*.rs"])
    proc = subprocess.run(
        args,
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    paths: set[str] = set()
    for line in proc.stdout.splitlines():
        _, _, tail = line.partition(":")
        if "grit_lib::" in tail:
            m = PATH_RE.search(tail)
            if m:
                paths.add(m.group(0))
    return paths


def last_ident(path: str) -> str:
    return path.split("::")[-1]


def restore_in_file(path: Path, ident: str) -> bool:
    text = path.read_text(encoding="utf-8")
    patterns = [
        (rf"^pub\(crate\) async fn {re.escape(ident)}\b", f"pub async fn {ident}"),
        (rf"^pub\(crate\) unsafe fn {re.escape(ident)}\b", f"pub unsafe fn {ident}"),
        (rf"^pub\(crate\) fn {re.escape(ident)}\b", f"pub fn {ident}"),
        (rf"^pub\(crate\) struct {re.escape(ident)}\b", f"pub struct {ident}"),
        (rf"^pub\(crate\) enum {re.escape(ident)}\b", f"pub enum {ident}"),
        (rf"^pub\(crate\) trait {re.escape(ident)}\b", f"pub trait {ident}"),
        (rf"^pub\(crate\) type {re.escape(ident)}\b", f"pub type {ident}"),
        (rf"^pub\(crate\) const {re.escape(ident)}\b", f"pub const {ident}"),
    ]
    new = text
    for pat, repl in patterns:
        updated, n = re.subn(pat, repl, new, count=1, flags=re.MULTILINE)
        if n:
            path.write_text(updated, encoding="utf-8")
            return True
    return False


def main() -> None:
    paths = collect_test_paths()
    idents = {last_ident(p) for p in paths if last_ident(p)[0].isascii()}
    restored = 0
    for ident in sorted(idents):
        proc = subprocess.run(
            [
                "rg",
                "-l",
                rf"pub\(crate\) (async fn|unsafe fn|fn|struct|enum|trait|type|const) {re.escape(ident)}\b",
                "grit-lib/src",
            ],
            cwd=ROOT,
            capture_output=True,
            text=True,
        )
        if proc.returncode != 0:
            continue
        for rel in proc.stdout.splitlines():
            if restore_in_file(ROOT / rel, ident):
                restored += 1
    print(f"restored {restored} items")


if __name__ == "__main__":
    main()
