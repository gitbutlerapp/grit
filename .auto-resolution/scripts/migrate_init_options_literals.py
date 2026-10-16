#!/usr/bin/env python3
"""Rewrite InitOptions struct literals in integration tests to builder calls."""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
ROOTS = [
    REPO / "grit-lib" / "tests",
    REPO / "grit-lib" / "benches",
    REPO / "grit-lib" / "examples",
    REPO / "grit-protocol" / "tests",
]

BLOCK = re.compile(
    r"&InitOptions\s*\{(?P<body>.*?)\.\.\s*InitOptions::default\(\)\s*\}",
    re.DOTALL,
)


def field_value(body: str, name: str) -> str | None:
    m = re.search(rf"{re.escape(name)}\s*:\s*(?P<v>[^,\n]+)", body)
    return m.group("v").strip() if m else None


def migrate_block(match: re.Match[str]) -> str:
    body = match.group("body")
    chain = ["InitOptions::new()"]
    if (bare := field_value(body, "bare")) is not None:
        chain.append(f".with_bare({bare})")
    if (branch := field_value(body, "initial_branch")) is not None:
        chain.append(f".with_initial_branch({branch})")
    if (storage := field_value(body, "ref_storage")) is not None:
        chain.append(f".with_ref_storage({storage})")
    if (template := field_value(body, "template_dir")) is not None:
        chain.append(f".with_template_dir({template})")
    if (sep := field_value(body, "separate_git_dir")) is not None:
        chain.append(f".with_separate_git_dir({sep})")
    if (bwt := field_value(body, "bare_with_worktree_at_path")) is not None:
        chain.append(f".with_bare_with_worktree_at_path({bwt})")
    if len(chain) == 1:
        return match.group(0)
    inner = "\n            ".join(chain)
    return f"&{inner}"


def migrate_file(path: Path) -> bool:
    text = path.read_text()
    new_text, n = BLOCK.subn(migrate_block, text)
    if n == 0:
        return False
    path.write_text(new_text)
    print(f"{path.relative_to(REPO)}: {n} block(s)")
    return True


def main() -> int:
    changed = 0
    for root in ROOTS:
        if not root.is_dir():
            continue
        for path in sorted(root.rglob("*.rs")):
            if migrate_file(path):
                changed += 1
    print(f"Updated {changed} file(s)")
    return 0


if __name__ == "__main__":
    sys.exit(main())
