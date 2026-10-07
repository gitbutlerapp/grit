"""Unit tests for deterministic blog generation."""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import blog  # noqa: E402


class BlogDeterminismTest(unittest.TestCase):
    def test_empty_feed_output_is_stable(self) -> None:
        first = blog.render_feed([])
        second = blog.render_feed([])
        self.assertEqual(first, second)
        self.assertIn("Thu, 01 Jan 1970 00:00:00 +0000", first)

    def test_load_posts_requires_date(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            content = Path(tmp)
            (content / "undated.md").write_text("---\ntitle: No date\n---\n\nHello.\n", encoding="utf-8")
            with mock.patch.object(blog, "CONTENT_DIR", content):
                with self.assertRaises(SystemExit):
                    blog.load_posts()


if __name__ == "__main__":
    unittest.main()
