#!/usr/bin/env python3
"""Drop candidate paths that integration tests or grit-cli still import."""

from __future__ import annotations

import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
PATH_RE = re.compile(r"grit_lib::[a-zA-Z0-9_]+(?:::[a-zA-Z0-9_]+)*")


def imported_paths() -> set[str]:
    args = [
        "rg",
        "-o",
        PATH_RE.pattern,
        "grit-lib/tests",
        "grit-examples",
        "grit-cli",
        "grit-protocol",
        "grit-http-server",
        "grit-utils",
        "-g",
        "*.rs",
    ]
    proc = subprocess.run(args, cwd=ROOT, capture_output=True, text=True, check=False)
    out: set[str] = set()
    for line in proc.stdout.splitlines():
        m = PATH_RE.search(line)
        if m:
            out.add(m.group(0))
    return out


def main() -> None:
    used = imported_paths()
    for line in sys.stdin:
        path = line.strip()
        if not path or path.startswith("#"):
            continue
        if any(u == path or u.startswith(path + "::") or path.startswith(u + "::") for u in used):
            continue
        print(path)


if __name__ == "__main__":
    main()
