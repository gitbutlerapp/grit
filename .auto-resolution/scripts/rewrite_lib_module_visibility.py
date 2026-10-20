#!/usr/bin/env python3
"""Rewrite grit-lib/src/lib.rs root module visibility for step 1306."""

from __future__ import annotations

import re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
LIB_RS = ROOT / "grit-lib" / "src" / "lib.rs"

# Allowed root `pub mod` names (prelude counts as one).
ROOT_PUBLIC = frozenset(
    {
        "apply",
        "blame",
        "bundle",
        "command_runner",
        "commit",
        "config",
        "diagnostics",
        "diff",
        "error",
        "index",
        "maintenance",
        "notes",
        "objects",
        "parallel",
        "porcelain",
        "progress",
        "refs",
        "remote",
        "repo",
        "repo_handles",
        "revwalk",
        "stash",
        "state",
        "worktree",
    }
)


def main() -> None:
    text = LIB_RS.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    out: list[str] = []
    i = 0
    while i < len(lines):
        line = lines[i]
        # Drop doc(hidden) escape hatches on internal bench/metrics modules.
        if line.strip() == "#[doc(hidden)]":
            peek = lines[i + 1].strip() if i + 1 < len(lines) else ""
            if peek.startswith("pub mod ewah_bench") or peek.startswith("pub mod zlib_inflate") or peek.startswith(
                "pub mod hot_path_test_metrics"
            ):
                i += 1
                continue

        m = re.match(r"^pub mod ([a-z_][a-z0-9_]*);\s*$", line)
        if m:
            name = m.group(1)
            if name in ROOT_PUBLIC:
                out.append(line)
            else:
                out.append(line.replace("pub mod", "pub(crate) mod", 1))
            i += 1
            continue

        # ewah_bench inline module block stays public for in-crate benches only via pub(crate) wrapper.
        if line.strip().startswith("pub mod ewah_bench {"):
            out.append("pub(crate) mod ewah_bench {\n")
            i += 1
            continue

        out.append(line)
        i += 1

    new_text = "".join(out)
    # Update stale crate-root comment about flat pub mod list.
    new_text = new_text.replace(
        "// --- Curated public API (additive; the flat `pub mod` list below is unchanged\n"
        "// so every existing `grit_lib::<module>::…` path keeps resolving) ---",
        "// --- Curated public API: domain roots and cross-cutting modules only.\n"
        "// Flat implementation modules are `pub(crate)`; use canonical domain paths. ---",
    )
    new_text = new_text.replace(
        "    pub use crate::odb::Odb;",
        "    pub use crate::objects::odb::Odb;",
    )
    LIB_RS.write_text(new_text, encoding="utf-8")
    print(f"Updated {LIB_RS.relative_to(ROOT)}")


if __name__ == "__main__":
    main()
