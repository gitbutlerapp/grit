#!/usr/bin/env python3
"""Restore `pub` on items that grit-lib integration tests still import."""

from __future__ import annotations

import re
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "grit-lib" / "src"

ITEM_RE = re.compile(
    r"error\[E0603\]: (?:function|struct|enum|trait|type|const|module) `(\w+)` is private"
)


def restore_ident(ident: str) -> bool:
    proc = subprocess.run(
        [
            "rg",
            "-l",
            rf"pub\(crate\) (async fn|unsafe fn|fn|struct|enum|trait|type|const|mod) {re.escape(ident)}\b",
            "grit-lib/src",
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=False,
    )
    restored = False
    for rel in proc.stdout.splitlines():
        path = ROOT / rel
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
            (rf"^pub\(crate\) mod {re.escape(ident)}\b", f"pub mod {ident}"),
        ]
        for pat, repl in patterns:
            updated, n = re.subn(pat, repl, text, count=1, flags=re.MULTILINE)
            if n:
                path.write_text(updated, encoding="utf-8")
                restored = True
                break
    return restored


def main() -> None:
    total = 0
    for _round in range(50):
        proc = subprocess.run(
            ["cargo", "test", "-p", "grit-lib", "--test", "integration", "--no-run"],
            cwd=ROOT,
            capture_output=True,
            text=True,
            check=False,
        )
        idents = sorted(set(ITEM_RE.findall(proc.stderr)))
        if not idents:
            print(f"integration test crate compiles after restoring {total} items")
            return
        round_restored = 0
        for ident in idents:
            if restore_ident(ident):
                round_restored += 1
                total += 1
        print(f"round {_round + 1}: {round_restored} restored ({len(idents)} errors)")
        if round_restored == 0:
            print("stuck; remaining:", idents[:20])
            raise SystemExit(1)


if __name__ == "__main__":
    main()
