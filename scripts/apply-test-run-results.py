#!/usr/bin/env python3
"""Merge a batch of harness run results into data/tests/<group>/<stem>.toml files."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from test_status import TESTS_DATA, group_from_stem, load_one, save  # noqa: E402

REPO = Path(__file__).resolve().parent.parent


def parse_run_file(path: Path) -> dict[str, tuple[int, int, int, str, int]]:
    """file -> (total, pass, fail, status, expect_failure)."""
    out: dict[str, tuple[int, int, int, str, int]] = {}
    with path.open(encoding="utf-8") as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            parts = line.split("\t")
            if len(parts) < 6:
                continue
            base, total_s, pass_s, fail_s, status, ef_s = parts[:6]
            out[base] = (
                int(total_s),
                int(pass_s),
                int(fail_s),
                status,
                int(ef_s),
            )
    return out


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "run_path",
        type=Path,
        help="TSV lines: file_base, tests_total, passed, failing, status, expect_failure",
    )
    parser.add_argument(
        "--data-dir",
        type=Path,
        default=None,
        metavar="PATH",
        help=f"Status TOML tree to read/update (default: {TESTS_DATA}).",
    )
    parser.add_argument(
        "--skip-dashboard",
        action="store_true",
        help="Deprecated no-op (dashboards are no longer regenerated here; run scripts/generate-dashboard-from-test-files.py).",
    )
    args = parser.parse_args()
    data_dir = args.data_dir
    run_path = args.run_path
    if not run_path.is_file():
        print(f"ERROR: {run_path} not found", file=sys.stderr)
        sys.exit(1)

    base_dir = data_dir if data_dir is not None else TESTS_DATA
    if not base_dir.is_dir() and data_dir is None:
        print(
            f"ERROR: {base_dir} missing. Run: python3 scripts/generate-test-files-catalog.py",
            file=sys.stderr,
        )
        sys.exit(1)

    updates = parse_run_file(run_path)
    for base, (total, pass_n, fail_n, status, ef) in updates.items():
        group = group_from_stem(base)
        existing = load_one(base, group, data_dir)
        if existing is None and data_dir is None:
            print(f"WARNING: unknown file {base!r} (not in catalog); skipping", file=sys.stderr)
            continue
        fields = existing or {"in_scope": "yes"}
        fields.update(
            {
                "tests_total": total,
                "passed_last": pass_n,
                "failing": fail_n,
                "status": status,
                "expect_failure": ef,
                "fully_passing": total > 0 and fail_n == 0,
            }
        )
        save(base, group, fields, data_dir)


if __name__ == "__main__":
    main()
