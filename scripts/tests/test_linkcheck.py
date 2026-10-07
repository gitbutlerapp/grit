"""Unit tests for scripts/linkcheck.py."""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path
from unittest import mock

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import linkcheck  # noqa: E402


class LinkcheckFixture(unittest.TestCase):
    def setUp(self) -> None:
        self._tmpdir = tempfile.TemporaryDirectory()
        self.site = Path(self._tmpdir.name)
        self._orig_site_root = linkcheck.SITE_ROOT
        linkcheck.SITE_ROOT = self.site

        (self.site / "index.html").write_text(
            '<a href="good/">ok</a><a href="https://example.com">ext</a>',
            encoding="utf-8",
        )
        good_dir = self.site / "good"
        good_dir.mkdir()
        (good_dir / "index.html").write_text('<h2 id="here">Here</h2>', encoding="utf-8")
        (self.site / "broken.html").write_text('<a href="missing.html">nope</a>', encoding="utf-8")
        (self.site / "anchor.html").write_text('<a href="good/#ghost">bad frag</a>', encoding="utf-8")

    def tearDown(self) -> None:
        linkcheck.SITE_ROOT = self._orig_site_root
        self._tmpdir.cleanup()

    def test_good_internal_link_passes(self) -> None:
        issues = linkcheck.run(check_external=False)
        self.assertFalse(any(i.source.name == "index.html" and i.raw == "good/" for i in issues))

    def test_missing_file_is_reported(self) -> None:
        issues = linkcheck.run(check_external=False)
        missing = [i for i in issues if "missing.html" in i.raw]
        self.assertEqual(len(missing), 1)
        self.assertIn("not found", missing[0].detail)

    def test_missing_anchor_is_reported(self) -> None:
        issues = linkcheck.run(check_external=False)
        anchors = [i for i in issues if "ghost" in i.raw]
        self.assertEqual(len(anchors), 1)
        self.assertIn("missing anchor", anchors[0].detail)

    def test_external_link_skipped_by_default(self) -> None:
        issues = linkcheck.run(check_external=False)
        self.assertFalse(any(i.raw.startswith("https://") for i in issues))

    def test_protocol_relative_external_normalizes_without_crash(self) -> None:
        self.assertEqual(
            linkcheck.normalize_external_url("//example.com/path"),
            "https://example.com/path",
        )
        proto_site = Path(self._tmpdir.name) / "proto-only"
        proto_site.mkdir()
        (proto_site / "page.html").write_text('<a href="//example.com">x</a>', encoding="utf-8")
        linkcheck.SITE_ROOT = proto_site
        with mock.patch("urllib.request.urlopen") as urlopen:
            urlopen.return_value.__enter__.return_value.status = 200
            issues = linkcheck.run(check_external=True)
        self.assertEqual(issues, [])
        urlopen.assert_called_once()
        request = urlopen.call_args[0][0]
        self.assertEqual(request.full_url, "https://example.com")


class LinkcheckNormalizeTest(unittest.TestCase):
    def test_unsupported_external_scheme_raises(self) -> None:
        with self.assertRaises(ValueError):
            linkcheck.normalize_external_url("ftp://example.com")


if __name__ == "__main__":
    unittest.main()
