#!/usr/bin/env python3
"""Run harness tests in parallel by Git test family (``t0``–``t9``).

Invokes ``scripts/run-tests.sh`` once per selected family with ``--no-catalog``.
Families write disjoint per-test status TOMLs under ``data/tests/``, so no
merge step is needed. The parent runs the catalog once up front and, when
``--dashboard`` is passed, the dashboard generator once at the end.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
from concurrent.futures import ThreadPoolExecutor, as_completed
from pathlib import Path

REPO = Path(__file__).resolve().parent.parent
RUN_TESTS = REPO / "scripts" / "run-tests.sh"
CATALOG = REPO / "scripts" / "generate-test-files-catalog.py"
GEN_DASH = REPO / "scripts" / "generate-dashboard-from-test-files.py"
BIN = REPO / "target" / "release" / "grit-git"
TESTS_DIR = REPO / "tests"


def family_digit_from_arg(raw: str) -> int | None:
    """Map a user target (``t1``, ``1``, ``t12-foo``) to a single family digit 0–9."""
    s = raw.strip()
    for prefix in ("tests/", "./tests/"):
        if s.startswith(prefix):
            s = s[len(prefix) :]
            break
    s = os.path.basename(s)
    if s.endswith(".sh"):
        s = s[:-3]
    if not s.startswith("t") or len(s) < 2:
        return None
    rest = s[1:]
    i = 0
    while i < len(rest) and rest[i].isdigit():
        i += 1
    digits = rest[:i]
    if not digits:
        return None
    return int(digits[0])


def families_from_positionals(positionals: list[str]) -> list[int] | None:
    """Return sorted unique family digits, or None if any arg requests a single-file run."""
    if not positionals:
        return list(range(10))
    out: set[int] = set()
    for raw in positionals:
        if raw.strip().endswith(".sh"):
            return None
        d = family_digit_from_arg(raw)
        if d is None:
            print(f"WARNING: could not map {raw!r} to a family; skipping", file=sys.stderr)
            continue
        out.add(d)
    if not out:
        print("ERROR: no valid family targets", file=sys.stderr)
        sys.exit(1)
    return sorted(out)


def prelude() -> None:
    if not BIN.is_file() or not os.access(BIN, os.X_OK):
        print(f"ERROR: grit binary not found or not executable: {BIN}", file=sys.stderr)
        print("Run: cargo build --release -p grit-git", file=sys.stderr)
        sys.exit(1)
    TESTS_DIR.mkdir(parents=True, exist_ok=True)
    shutil.copy2(BIN, TESTS_DIR / "grit")
    os.chmod(TESTS_DIR / "grit", 0o755)
    subprocess.run([sys.executable, str(CATALOG)], cwd=str(REPO), check=True)


def run_one_family(
    digit: int,
    timeout: int,
    quiet: bool,
    verbose: bool,
    from_stem: str,
) -> tuple[int, int]:
    cmd = [
        str(RUN_TESTS),
        "--no-catalog",
        "--family",
        str(digit),
        "--timeout",
        str(timeout),
    ]
    if quiet:
        cmd.append("--quiet")
    if verbose:
        cmd.append("--verbose")
    if from_stem:
        cmd.extend(["--from", from_stem])
    env = os.environ.copy()
    env.pop("GRIT_FAMILY_FILTER", None)
    proc = subprocess.run(cmd, cwd=str(REPO), env=env)
    return digit, proc.returncode


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--timeout", type=int, default=120, metavar="N")
    parser.add_argument("--quiet", action="store_true")
    parser.add_argument(
        "--verbose",
        "-v",
        action="store_true",
        help="Forward to run-tests.sh: print each file as it starts.",
    )
    parser.add_argument("--from", dest="from_stem", default="", metavar="NAME")
    parser.add_argument(
        "--dashboard",
        action="store_true",
        help="Regenerate docs/ dashboards once after all families finish (off by default).",
    )
    parser.add_argument(
        "targets",
        nargs="*",
        help="Optional family targets (e.g. t1 t2). Default: all t0–t9.",
    )
    args = parser.parse_args()

    if args.quiet and args.verbose:
        print("ERROR: --verbose and --quiet are mutually exclusive", file=sys.stderr)
        sys.exit(1)

    if args.targets and any(t.strip().endswith(".sh") for t in args.targets):
        cmd = [str(RUN_TESTS), "--timeout", str(args.timeout)]
        if args.quiet:
            cmd.append("--quiet")
        if args.verbose:
            cmd.append("--verbose")
        if args.dashboard:
            cmd.append("--dashboard")
        if args.from_stem:
            cmd.extend(["--from", args.from_stem])
        cmd.extend(args.targets)
        raise SystemExit(subprocess.run(cmd, cwd=str(REPO)).returncode)

    fams = families_from_positionals(args.targets)

    prelude()

    failed: list[int] = []
    with ThreadPoolExecutor(max_workers=len(fams)) as pool:
        futures = [
            pool.submit(
                run_one_family,
                d,
                args.timeout,
                args.quiet,
                args.verbose,
                args.from_stem,
            )
            for d in fams
        ]
        for fut in as_completed(futures):
            digit, code = fut.result()
            if code != 0:
                failed.append(digit)
                print(f"ERROR: family t{digit} run-tests.sh exited with {code}", file=sys.stderr)

    if failed:
        sys.exit(1)

    if args.dashboard:
        subprocess.run([sys.executable, str(GEN_DASH)], cwd=str(REPO), check=True)
    if not args.quiet:
        suffix = " and dashboards" if args.dashboard else ""
        print(f"Updated {REPO / 'data' / 'tests'}{suffix}.")


if __name__ == "__main__":
    main()
