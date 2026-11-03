"""Unit tests for the public API test-item word splitter (scripts/public_api.py)."""
from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import public_api as pa  # noqa: E402


class PublicApiTestItemWordSplitterTests(unittest.TestCase):
    def test_notes_tree_does_not_match(self) -> None:
        self.assertFalse(pa.path_violates_test_item_rule("grit_lib::notes::NotesTree"))

    def test_write_stats_does_not_match(self) -> None:
        self.assertFalse(pa.path_violates_test_item_rule("grit_lib::objects::WriteStats"))

    def test_composite_store_does_not_match(self) -> None:
        self.assertFalse(
            pa.path_violates_test_item_rule("grit_lib::objects::CompositeStore")
        )

    def test_reflog_rewrite_stale_does_not_match(self) -> None:
        self.assertFalse(
            pa.path_violates_test_item_rule("grit_lib::refs::ReflogRewriteStale")
        )

    def test_bench_fixture_matches(self) -> None:
        self.assertTrue(pa.path_violates_test_item_rule("grit_lib::bench_fixture"))

    def test_create_for_probe_matches(self) -> None:
        self.assertTrue(
            pa.path_violates_test_item_rule(
                "grit_lib::remote::HttpClientFactory::create_for_probe"
            )
        )

    def test_segment_words_notes_tree(self) -> None:
        self.assertEqual(pa.segment_words("NotesTree"), ["notes", "tree"])


if __name__ == "__main__":
    unittest.main()
