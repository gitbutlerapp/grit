"""Unit tests for benchmark page generation from grit-bench JSON."""
from __future__ import annotations

import json
import shutil
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import benchpage  # noqa: E402
import docs  # noqa: E402


FIXTURE_REPORT = {
    "schema_version": 1,
    "timestamp": "2026-10-07T12:00:00Z",
    "machine": {
        "cpu_model": "Test CPU",
        "physical_cores": 4,
        "logical_cores": 8,
        "ram_bytes": 16000000000,
        "os": "linux",
        "kernel": "Linux 6.12 test",
        "scratch_filesystem": "tmpfs",
        "rustc_version": "rustc 1.99.0",
        "cargo_profile": "release",
    },
    "tools": {"git": "git version 2.43.0", "grit": "grit 0.5.0", "grit_commit": None},
    "scenarios": [
        {
            "id": "status-1000",
            "group": "status",
            "fixture": "synthetic-1000",
            "description": "status with dirty index",
            "driver": "cli",
            "git": {
                "mean_ms": 100.0,
                "median_ms": 99.0,
                "stddev_ms": 5.0,
                "min_ms": 90.0,
                "max_ms": 110.0,
                "runs_ms": [100.0],
            },
            "grit": {
                "mean_ms": 150.0,
                "median_ms": 148.0,
                "stddev_ms": 6.0,
                "min_ms": 140.0,
                "max_ms": 160.0,
                "runs_ms": [150.0],
            },
            "ratio": 1.48,
        },
        {
            "id": "add-1000",
            "group": "add",
            "fixture": "synthetic-1000",
            "description": "add one file",
            "driver": "cli",
            "git": {
                "mean_ms": 50.0,
                "median_ms": 49.0,
                "stddev_ms": 2.0,
                "min_ms": 48.0,
                "max_ms": 52.0,
                "runs_ms": [50.0],
            },
            "grit": {
                "mean_ms": 120.0,
                "median_ms": 119.0,
                "stddev_ms": 3.0,
                "min_ms": 115.0,
                "max_ms": 125.0,
                "runs_ms": [120.0],
            },
            "ratio": 2.4,
        },
        {
            "id": "status-100",
            "group": "status",
            "fixture": "synthetic-100",
            "description": "status clean",
            "driver": "cli",
            "git": {
                "mean_ms": 10.0,
                "median_ms": 10.0,
                "stddev_ms": 1.0,
                "min_ms": 9.0,
                "max_ms": 11.0,
                "runs_ms": [10.0],
            },
            "grit": {
                "mean_ms": 8.0,
                "median_ms": 8.0,
                "stddev_ms": 0.5,
                "min_ms": 7.5,
                "max_ms": 8.5,
                "runs_ms": [8.0],
            },
            "ratio": 0.8,
        },
    ],
}


class BenchpageTest(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="grit-benchpage-test-"))
        self.addCleanup(lambda: shutil.rmtree(self.tmp, ignore_errors=True))
        self.baseline = self.tmp / "fixture.json"
        self.baseline.write_text(json.dumps(FIXTURE_REPORT), encoding="utf-8")

    def test_ratio_from_means(self) -> None:
        bundle = benchpage.merge_baselines([self.baseline])
        by_id = {s.id: s for s in bundle.scenarios}
        self.assertAlmostEqual(by_id["status-1000"].ratio, 1.5)
        self.assertAlmostEqual(by_id["add-1000"].ratio, 2.4)

    def test_sort_order(self) -> None:
        bundle = benchpage.merge_baselines([self.baseline])
        ids = [s.id for s in bundle.scenarios]
        self.assertEqual(ids, ["add-1000", "status-100", "status-1000"])

    def test_slow_flag_threshold(self) -> None:
        bundle = benchpage.merge_baselines([self.baseline])
        html_out = benchpage.render_tables(bundle)
        self.assertIn('class="bench-slow"', html_out)
        self.assertIn("2.40×", html_out)

    def test_missing_baseline_errors(self) -> None:
        with self.assertRaises(SystemExit) as ctx:
            benchpage.load_report(self.tmp / "missing.json")
        self.assertIn("missing", str(ctx.exception))

    def test_malformed_json_errors(self) -> None:
        bad = self.tmp / "bad.json"
        bad.write_text("{not json", encoding="utf-8")
        with self.assertRaises(SystemExit) as ctx:
            benchpage.load_report(bad)
        self.assertIn("not valid JSON", str(ctx.exception))

    def test_malformed_scenario_errors(self) -> None:
        broken = dict(FIXTURE_REPORT)
        broken["scenarios"] = [{}]
        path = self.tmp / "broken-scenario.json"
        path.write_text(json.dumps(broken), encoding="utf-8")
        with self.assertRaises(SystemExit) as ctx:
            benchpage.merge_baselines([path])
        msg = str(ctx.exception)
        self.assertIn("broken-scenario.json", msg)
        self.assertIn("scenario[0]", msg)
        self.assertIn("missing", msg)

    def test_top_level_array_baseline_errors(self) -> None:
        path = self.tmp / "array-root.json"
        path.write_text("[]", encoding="utf-8")
        with self.assertRaises(SystemExit) as ctx:
            benchpage.load_report(path)
        msg = str(ctx.exception)
        self.assertIn("array-root.json", msg)
        self.assertIn("JSON object", msg)

    def test_null_machine_baseline_errors(self) -> None:
        broken = dict(FIXTURE_REPORT)
        broken["machine"] = None
        path = self.tmp / "null-machine.json"
        path.write_text(json.dumps(broken), encoding="utf-8")
        with self.assertRaises(SystemExit) as ctx:
            benchpage.load_report(path)
        msg = str(ctx.exception)
        self.assertIn("null-machine.json", msg)
        self.assertIn("'machine'", msg)
        self.assertIn("object", msg)

    def test_criterion_driver_grit_only(self) -> None:
        report = {
            **FIXTURE_REPORT,
            "scenarios": [
                {
                    "id": "delta-encode-text-256k",
                    "group": "delta_encode",
                    "fixture": "synthetic-256k-text",
                    "description": "Criterion only",
                    "driver": "criterion",
                    "grit": {
                        "mean_ms": 48.0,
                        "median_ms": 48.0,
                        "stddev_ms": 1.0,
                        "min_ms": 47.0,
                        "max_ms": 49.0,
                        "runs_ms": [48.0],
                    },
                }
            ],
        }
        path = self.tmp / "criterion.json"
        path.write_text(json.dumps(report), encoding="utf-8")
        bundle = benchpage.merge_baselines([path])
        self.assertEqual(len(bundle.scenarios), 1)
        self.assertIsNone(bundle.scenarios[0].git_mean_ms)
        self.assertIsNone(bundle.scenarios[0].ratio)
        html_out = benchpage.render_tables(bundle)
        self.assertIn("—", html_out)

    def test_scenario_row_count_matches_baseline(self) -> None:
        bundle = benchpage.merge_baselines([self.baseline])
        html_out = benchpage.render_tables(bundle)
        self.assertEqual(benchpage.count_data_rows(html_out), len(FIXTURE_REPORT["scenarios"]))

    def test_benchmark_markdown_tables(self) -> None:
        bundle = benchpage.merge_baselines([self.baseline])
        md_out = benchpage.render_tables_markdown(bundle)
        self.assertIn("## Summary by operation", md_out)
        self.assertIn("| Scenario | Fixture |", md_out)
        self.assertIn("`status-1000`", md_out)
        self.assertNotIn("<table", md_out)

    def test_benchmark_markdown_for_manifest_matches_scenarios(self) -> None:
        manifest = ROOT / "content" / "docs" / "site.toml"
        paths = benchpage.load_baseline_paths(manifest)
        expected = sum(len(benchpage.load_report(p)["scenarios"]) for p in paths)
        md_out = benchpage.benchmark_markdown_for_manifest(manifest)
        self.assertEqual(md_out.count("| `"), expected)

    def test_committed_baselines_match_rendered_rows(self) -> None:
        manifest = ROOT / "content" / "docs" / "site.toml"
        paths = benchpage.load_baseline_paths(manifest)
        for path in paths:
            self.assertTrue(path.is_file(), f"baseline on main missing: {path}")
        expected = sum(len(benchpage.load_report(p)["scenarios"]) for p in paths)
        html_out = benchpage.benchmark_html_for_manifest(manifest)
        self.assertEqual(benchpage.count_data_rows(html_out), expected)

    def test_docs_check_fails_when_baseline_tampered(self) -> None:
        baseline_path = ROOT / "grit-utils" / "baselines" / "hot-paths-after.json"
        if not baseline_path.is_file():
            self.skipTest("no committed baseline on main yet")
        original = baseline_path.read_text(encoding="utf-8")
        data = json.loads(original)
        data["scenarios"][0]["git"]["mean_ms"] = 0.001
        baseline_path.write_text(json.dumps(data), encoding="utf-8")
        self.addCleanup(lambda: baseline_path.write_text(original, encoding="utf-8"))
        with tempfile.TemporaryDirectory(prefix="grit-docs-check-bench-") as tmp:
            out = Path(tmp) / "docs"
            docs.generate(out, llms_dir=Path(tmp) / "llms")
            issues = __import__("site_util").compare_directories(out, ROOT / "docs" / "docs", label="docs")
            self.assertTrue(issues, "expected stale docs after baseline edit")


if __name__ == "__main__":
    unittest.main()
