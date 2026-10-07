#!/usr/bin/env python3
"""Regenerate or verify the generated docs and blog under docs/."""
from __future__ import annotations

import argparse
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPTS = ROOT / "scripts"


def run_script(name: str, *extra: str) -> int:
    result = subprocess.run([sys.executable, str(SCRIPTS / name), *extra], cwd=ROOT)
    return result.returncode


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="Render into a temp dir and fail if committed docs/docs/ or docs/blog/ is stale.",
    )
    args = parser.parse_args(argv)

    if args.check:
        code = run_script("docs.py", "--check")
        if code:
            return code
        return run_script("blog.py", "--check")

    code = run_script("docs.py")
    if code:
        return code
    return run_script("blog.py")


if __name__ == "__main__":
    raise SystemExit(main())
