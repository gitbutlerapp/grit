"""Unit tests for scripts/coverage.py."""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import coverage as cov  # noqa: E402


def _sample_json() -> str:
    files = [
        ("odb.rs", 100, 85),
        ("pack.rs", 200, 160),
        ("pack_index.rs", 50, 45),
        ("midx.rs", 80, 50),
        ("commit_graph_file.rs", 40, 12),
        ("commit_graph_write.rs", 30, 20),
        ("bloom.rs", 20, 10),
    ]
    payload_files = []
    for name, count, covered in files:
        payload_files.append(
            {
                "filename": str(ROOT / "grit-lib" / "src" / name),
                "summary": {
                    "lines": {
                        "count": count,
                        "covered": covered,
                        "percent": 100.0 * covered / count,
                    }
                },
            }
        )
    return json.dumps({"data": [{"files": payload_files}]})


class CoverageScriptTests(unittest.TestCase):
    def test_parse_coverage_json_maps_grit_lib_src_files(self) -> None:
        stats = cov.parse_coverage_json(_sample_json())
        self.assertIn("odb.rs", stats)
        self.assertEqual(stats["odb.rs"].count, 100)
        self.assertEqual(stats["odb.rs"].covered, 85)

    def test_aggregate_weighted_by_lines(self) -> None:
        stats = cov.parse_coverage_json(_sample_json())
        pack = cov.aggregate(["pack.rs", "pack_index.rs"], stats)
        self.assertEqual(pack.count, 250)
        self.assertEqual(pack.covered, 205)
        self.assertAlmostEqual(pack.percent, 82.0)

    def test_ratchet_floor_minus_two_rounded_down(self) -> None:
        self.assertEqual(cov.ratchet_floor(81.5), 79.5)
        self.assertEqual(cov.ratchet_floor(40.4), 38.4)
        self.assertEqual(cov.ratchet_floor(8.0), 6.0)

    def test_update_never_lowers_floors(self) -> None:
        floors_path = ROOT / "grit-lib" / "coverage-floors.toml"
        floors = cov.load_floors(floors_path)
        stats = cov.parse_coverage_json(_sample_json())
        # Artificially high floors on one file.
        floors.files["odb.rs"] = 99.0
        updated = cov.apply_update(floors, stats)
        self.assertGreaterEqual(updated.files["odb.rs"], 99.0)

    def test_check_fails_when_below_floor(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            floors_path = Path(tmp) / "floors.toml"
            floors_path.write_text(
                """
[meta]
updated = "test"

[core]
groups = ["odb"]
minimum = 99.0

[group.odb]
files = ["odb.rs"]
minimum = 99.0

[file."odb.rs"]
minimum = 99.0
""",
                encoding="utf-8",
            )
            floors = cov.load_floors(floors_path)
            stats = cov.parse_coverage_json(_sample_json())
            _table, failures = cov.check(floors, stats)
            self.assertTrue(failures)
            self.assertTrue(any("odb.rs" in f for f in failures))


if __name__ == "__main__":
    unittest.main()
