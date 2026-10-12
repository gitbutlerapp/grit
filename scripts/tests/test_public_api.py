"""Unit tests for scripts/public_api.py and public-api.sh pins."""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import public_api as pa  # noqa: E402


class PublicApiScriptTests(unittest.TestCase):
    def test_parse_pins_matches_public_api_sh(self) -> None:
        pins = pa.parse_pins()
        self.assertEqual(pins["nightly_date"], "2026-10-01")
        self.assertEqual(pins["cargo_public_api_version"], "0.52.0")

    def test_compare_snapshot_files_match(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            left = Path(tmp) / "left.txt"
            right = Path(tmp) / "right.txt"
            left.write_text("pub mod grit_lib\n", encoding="utf-8")
            right.write_text("pub mod grit_lib\n", encoding="utf-8")
            ok, diff = pa.compare_snapshot_files(left, right)
            self.assertTrue(ok)
            self.assertEqual(diff, "")

    def test_compare_snapshot_files_differs(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            left = Path(tmp) / "left.txt"
            right = Path(tmp) / "right.txt"
            left.write_text("pub mod grit_lib\n", encoding="utf-8")
            right.write_text("pub mod grit_lib\npub fn dummy()\n", encoding="utf-8")
            ok, diff = pa.compare_snapshot_files(left, right)
            self.assertFalse(ok)
            self.assertIn("dummy", diff)


if __name__ == "__main__":
    unittest.main()
