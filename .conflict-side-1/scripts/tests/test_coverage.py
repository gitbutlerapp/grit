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
        ("odb/mod.rs", 100, 85),
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
        self.assertIn("odb/mod.rs", stats)
        self.assertEqual(stats["odb/mod.rs"].count, 100)
        self.assertEqual(stats["odb/mod.rs"].covered, 85)

    def test_parse_coverage_json_normalizes_parent_path_segments(self) -> None:
        payload = {
            "data": [
                {
                    "files": [
                        {
                            "filename": str(
                                ROOT
                                / "grit-lib"
                                / "src"
                                / "diff"
                                / ".."
                                / "structured_diff.rs"
                            ),
                            "summary": {
                                "lines": {
                                    "count": 100,
                                    "covered": 80,
                                    "percent": 80.0,
                                }
                            },
                        }
                    ]
                }
            ]
        }
        stats = cov.parse_coverage_json(json.dumps(payload))
        self.assertIn("structured_diff.rs", stats)
        self.assertEqual(stats["structured_diff.rs"].count, 100)
        self.assertEqual(stats["structured_diff.rs"].covered, 80)

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
        floors.files["odb/mod.rs"] = 99.0
        updated = cov.apply_update(floors, stats)
        self.assertGreaterEqual(updated.files["odb/mod.rs"], 99.0)

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
files = ["odb/mod.rs"]
minimum = 99.0

[file."odb/mod.rs"]
minimum = 99.0
""",
                encoding="utf-8",
            )
            floors = cov.load_floors(floors_path)
            stats = cov.parse_coverage_json(_sample_json())
            _table, failures = cov.check(floors, stats)
            self.assertTrue(failures)
            self.assertTrue(any("odb/mod.rs" in f for f in failures))

    def test_load_floors_legacy_core_table(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            floors_path = Path(tmp) / "floors.toml"
            floors_path.write_text(
                """
[meta]
updated = "test"

[core]
groups = ["odb", "pack"]
minimum = 80.0

[group.odb]
files = ["odb.rs"]
minimum = 80.0

[group.pack]
files = ["pack.rs"]
minimum = 75.0
""",
                encoding="utf-8",
            )
            floors = cov.load_floors(floors_path)
            self.assertEqual(floors.core_groups, ["odb", "pack"])
            self.assertEqual(floors.core_minimum, 80.0)
            stats = cov.parse_coverage_json(_sample_json())
            table, failures = cov.check(floors, stats)
            self.assertFalse(failures)
            self.assertTrue(any(line.startswith("core ") for line in table))

    def test_named_aggregate_groups_and_minimum(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            floors_path = Path(tmp) / "floors.toml"
            floors_path.write_text(
                """
[meta]
updated = "test"

[core]
groups = ["odb"]
minimum = 50.0

[aggregate.refs-config]
groups = ["refs", "reflog"]
minimum = 70.0

[group.refs]
files = ["odb.rs"]
minimum = 50.0

[group.reflog]
files = ["pack.rs"]
minimum = 50.0

[group.odb]
files = ["odb.rs"]
minimum = 50.0
""",
                encoding="utf-8",
            )
            floors = cov.load_floors(floors_path)
            self.assertIn("refs-config", floors.aggregates)
            self.assertEqual(floors.aggregates["refs-config"][0], ["refs", "reflog"])
            self.assertEqual(floors.aggregates["refs-config"][1], 70.0)
            stats = cov.parse_coverage_json(_sample_json())
            agg_files = cov.aggregate_group_files(
                ["refs", "reflog"], floors.groups
            )
            expected = cov.aggregate(agg_files, stats)
            table, failures = cov.check(floors, stats)
            self.assertFalse(failures)
            self.assertTrue(
                any(
                    line.startswith("aggregate:refs-config")
                    and f"{expected.percent:.1f}" in line
                    for line in table
                )
            )

    def test_referenced_module_omitted_from_json_not_zero_filled(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "grit-lib" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod shim;\nmod included;\n", encoding="utf-8")
            (src / "shim.rs").write_text("pub fn shim_only() {}\n", encoding="utf-8")
            (src / "included.rs").write_text("pub fn covered() {}\n", encoding="utf-8")
            stats = {"included.rs": cov.LineStats(count=10, covered=8)}
            complete = cov.complete_library_stats(stats, src_root=src)
            self.assertNotIn("shim.rs", complete)
            total = cov.library_total_stats(stats, src_root=src)
            self.assertEqual(total.count, 10)
            self.assertEqual(total.covered, 8)

    def test_total_includes_omitted_source_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            src = Path(tmp) / "grit-lib" / "src"
            src.mkdir(parents=True)
            (src / "lib.rs").write_text("mod included;\n", encoding="utf-8")
            (src / "included.rs").write_text("pub fn covered() {}\n", encoding="utf-8")
            (src / "orphan.rs").write_text("pub fn orphan() {}\n", encoding="utf-8")
            stats = {"included.rs": cov.LineStats(count=1, covered=1)}
            missing = cov.llvm_cov_missing_src_files(stats, src_root=src)
            self.assertIn("orphan.rs", missing)
            self.assertNotIn("included.rs", missing)
            keys_only = cov.aggregate(list(stats.keys()), stats)
            full_total = cov.library_total_stats(stats, src_root=src)
            self.assertLess(full_total.percent, keys_only.percent)

    def test_committed_coverage_gate_minimums(self) -> None:
        floors = cov.load_floors(ROOT / "grit-lib" / "coverage-floors.toml")
        self.assertEqual(floors.total_minimum, 80.0)
        self.assertIn("history", floors.aggregates)
        self.assertEqual(floors.aggregates["history"][1], 85.0)
        for name in ("diff", "revwalk", "revparse", "index", "index-ext", "merge", "stash-notes-worktree"):
            self.assertIn(name, floors.groups)
            minimum = floors.groups[name][1]
            self.assertIsNotNone(minimum)
            assert minimum is not None
            self.assertGreaterEqual(minimum, 85.0)

    def test_total_floor_fails_at_79_9_percent(self) -> None:
        """79.9% total must fail against the committed 80% floor via check()."""
        floors_path = ROOT / "grit-lib" / "coverage-floors.toml"
        floors = cov.load_floors(floors_path)
        self.assertIsNotNone(floors.total_minimum)
        assert floors.total_minimum is not None
        self.assertEqual(floors.total_minimum, 80.0)

        src = ROOT / "grit-lib" / "src"
        payload_files = []
        for rel in cov.grit_lib_src_rs_files(src_root=src):
            line_count = cov.count_source_lines_for_coverage(src / rel)
            if line_count == 0:
                continue
            payload_files.append(
                {
                    "filename": str(src / rel),
                    "summary": {
                        "lines": {
                            "count": line_count,
                            "covered": 0,
                            "percent": 0.0,
                        }
                    },
                }
            )
        total_lines = sum(
            int(item["summary"]["lines"]["count"]) for item in payload_files
        )
        covered_target = int(round(total_lines * 0.799))
        remaining = covered_target
        for item in payload_files:
            count = int(item["summary"]["lines"]["count"])
            take = min(count, remaining)
            item["summary"]["lines"]["covered"] = take
            remaining -= take
            if remaining <= 0:
                break
        stats = cov.parse_coverage_json(json.dumps({"data": [{"files": payload_files}]}))
        total_stats = cov.library_total_stats(stats, src_root=src)
        self.assertAlmostEqual(total_stats.percent, 79.9, delta=0.15)

        _table, failures = cov.check(floors, stats, validate_src_tree=True)
        self.assertTrue(any("total:" in f for f in failures))

    def test_history_aggregate_floor_fails_below_85(self) -> None:
        floors = cov.load_floors(ROOT / "grit-lib" / "coverage-floors.toml")
        history_groups, history_minimum = floors.aggregates["history"]
        self.assertEqual(history_minimum, 85.0)
        stats = cov.parse_coverage_json(_sample_json())
        agg_files: list[str] = []
        for group_name in history_groups:
            agg_files.extend(floors.groups[group_name][0])
        for name in agg_files:
            if name not in stats:
                stats[name] = cov.LineStats(count=100, covered=80)
        st = cov.aggregate(agg_files, stats)
        self.assertLess(st.percent, 85.0)
        _table, failures = cov.check(floors, stats)
        self.assertTrue(any("aggregate history" in f for f in failures))

    def test_total_floor_failure(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            floors_path = Path(tmp) / "floors.toml"
            floors_path.write_text(
                """
[meta]
updated = "test"

[total]
minimum = 99.0

[core]
groups = ["odb"]
minimum = 50.0

[group.odb]
files = ["odb/mod.rs"]
minimum = 50.0

[file."odb/mod.rs"]
minimum = 50.0
""",
                encoding="utf-8",
            )
            floors = cov.load_floors(floors_path)
            self.assertEqual(floors.total_minimum, 99.0)
            stats = cov.parse_coverage_json(_sample_json())
            _table, failures = cov.check(floors, stats)
            self.assertTrue(any("total:" in f for f in failures))

    def test_update_ratchet_total_never_lowers(self) -> None:
        floors_path = ROOT / "grit-lib" / "coverage-floors.toml"
        floors = cov.load_floors(floors_path)
        stats = cov.parse_coverage_json(_sample_json())
        if floors.total_minimum is None:
            floors.total_minimum = 90.0
        else:
            floors.total_minimum = max(floors.total_minimum, 90.0)
        updated = cov.apply_update(floors, stats)
        self.assertGreaterEqual(updated.total_minimum, floors.total_minimum)

    def test_aggregate_failure_message(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            floors_path = Path(tmp) / "floors.toml"
            floors_path.write_text(
                """
[meta]
updated = "test"

[core]
groups = ["odb"]
minimum = 50.0

[aggregate.strict]
groups = ["refs"]
minimum = 99.0

[group.refs]
files = ["odb/mod.rs"]
minimum = 50.0

[group.odb]
files = ["odb/mod.rs"]
minimum = 50.0
""",
                encoding="utf-8",
            )
            floors = cov.load_floors(floors_path)
            stats = cov.parse_coverage_json(_sample_json())
            _table, failures = cov.check(floors, stats)
            self.assertTrue(any("aggregate strict" in f for f in failures))


if __name__ == "__main__":
    unittest.main()
