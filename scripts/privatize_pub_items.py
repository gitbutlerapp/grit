#!/usr/bin/env python3
"""Make selected `pub` items `pub(crate)` in Rust sources (visibility sweep helper)."""

from __future__ import annotations

import argparse
import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

# Match item-level pub at line start (not pub mod, pub use, pub(in ...)).
ITEM_PUB = re.compile(
    r"^(?P<indent>\s*)pub(?P<in_path>\([^)]*\))?\s+"
    r"(?P<kind>fn|struct|enum|type|const|static|trait)\s+"
    r"(?P<name>[A-Za-z_][A-Za-z0-9_]*)"
)


def load_keep(path: Path) -> set[str]:
    names: set[str] = set()
    for line in path.read_text(encoding="utf-8").splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        names.add(line.split()[0])
    return names


def privatize_file(path: Path, keep: set[str], dry_run: bool) -> int:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    changed = 0
    out: list[str] = []
    for line in lines:
        stripped = line.lstrip()
        if stripped.startswith("pub use ") or stripped.startswith("pub mod "):
            out.append(line)
            continue
        m = ITEM_PUB.match(line)
        if m and m.group("in_path") is None and m.group("name") not in keep:
            new_line = line.replace("pub ", "pub(crate) ", 1)
            if new_line != line:
                changed += 1
                line = new_line
        out.append(line)
    if changed and not dry_run:
        path.write_text("".join(out), encoding="utf-8")
    return changed


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--keep", type=Path, required=True)
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("paths", nargs="+", type=Path)
    args = parser.parse_args()
    keep = load_keep(args.keep)
    total = 0
    for rel in args.paths:
        path = rel if rel.is_absolute() else ROOT / rel
        n = privatize_file(path, keep, args.dry_run)
        if n:
            print(f"{path.relative_to(ROOT)}: {n} items privatized")
        total += n
    print(f"total privatized: {total}")


if __name__ == "__main__":
    main()
