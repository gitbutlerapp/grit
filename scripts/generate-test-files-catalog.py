#!/usr/bin/env python3
"""Build or merge data/tests/<group>/<stem>.toml status files from tests/t*.sh.

Scans tests/ for harness files, assigns ``group`` from the first decimal digit
after ``t`` (Git upstream families; see ``git/t/README`` “Naming Tests”):
``t0``–``t9``. Counts test markers per file and merges with any existing status
TOMLs so run results are preserved for files that still exist; TOMLs for
removed test files are pruned.
"""

from __future__ import annotations

import re
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_status import TESTS_DATA, group_from_stem, load_all, prune, save  # noqa: E402

REPO = Path(__file__).resolve().parent.parent
TESTS_DIR = REPO / "tests"

FILE_RE = re.compile(r"^t\d+.+\.sh$")


def count_expects_and_group(sh_path: Path) -> tuple[str, int, int]:
    """Return (group, test markers count, test_expect_failure count)."""
    stem = sh_path.stem
    group = group_from_stem(stem)
    try:
        text = sh_path.read_text(encoding="utf-8", errors="replace")
    except OSError:
        return group, 0, 0
    markers = len(
        re.findall(r"\btest_expect_success\b|\btest_expect_failure\b", text)
    )
    ef = len(re.findall(r"\btest_expect_failure\b", text))
    return group, markers, ef


def main() -> None:
    existing = load_all()
    discovered: list[str] = []
    for p in sorted(TESTS_DIR.glob("t*.sh")):
        if FILE_RE.match(p.name):
            discovered.append(p.stem)

    for base in discovered:
        sh = TESTS_DIR / f"{base}.sh"
        group, marker_count, expect_failure = count_expects_and_group(sh)
        prev = existing.get(base)
        if prev is not None:
            in_scope = prev["in_scope"]
            # Run metrics come from apply-test-run-results.py after harness runs.
            # Keep tests_total from the last harness merge when present; do not
            # replace it with the static marker count (regex can under-count, and
            # resetting tests_total while preserving passed_last corrupts the row).
            passed_last = prev["passed_last"]
            failing = prev["failing"]
            fully_passing = prev["fully_passing"]
            status = prev["status"]
            tests_total = prev["tests_total"] if prev["tests_total"] > 0 else marker_count
            # Repair rows where an older catalog run reset tests_total to the marker
            # count but left pass/fail from the harness (totals must agree).
            run_sum = passed_last + failing
            if tests_total != run_sum and run_sum > 0:
                tests_total = run_sum
        else:
            in_scope = "yes"
            passed_last = 0
            failing = 0
            fully_passing = False
            status = ""
            tests_total = marker_count

        save(
            base,
            group,
            {
                "in_scope": in_scope,
                "tests_total": tests_total,
                "passed_last": passed_last,
                "failing": failing,
                "fully_passing": fully_passing,
                "status": status,
                "expect_failure": expect_failure,
            },
        )

    removed = prune(set(discovered))
    note = f", pruned {len(removed)} stale" if removed else ""
    print(f"Wrote {len(discovered)} status TOMLs under {TESTS_DATA}{note}")


if __name__ == "__main__":
    main()
