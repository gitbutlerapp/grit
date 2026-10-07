"""Unit tests for the sectioned docs generator."""
from __future__ import annotations

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import docs  # noqa: E402


class DocsManifestTest(unittest.TestCase):
    def setUp(self) -> None:
        self.content = Path(tempfile.mkdtemp(prefix="grit-docs-test-"))
        self.addCleanup(lambda: shutil.rmtree(self.content, ignore_errors=True))
        shutil.copytree(ROOT / "content" / "docs", self.content, dirs_exist_ok=True)

    def test_missing_manifest_page_fails(self) -> None:
        manifest = self.content / "site.toml"
        text = manifest.read_text(encoding="utf-8")
        manifest.write_text(
            text
            + """

[[section.page]]
file = "does-not-exist.md"
slug = "missing"
label = "Missing"
""",
            encoding="utf-8",
        )
        sections = docs.load_manifest_from(manifest, self.content)
        with self.assertRaises(SystemExit) as ctx:
            docs.validate_manifest(sections, content_dir=self.content)
        self.assertIn("missing page", str(ctx.exception))

    def test_unlisted_markdown_fails(self) -> None:
        (self.content / "orphan.md").write_text("---\ntitle: Orphan\n---\n\nHi.\n", encoding="utf-8")
        sections = docs.load_manifest_from(self.content / "site.toml", self.content)
        with self.assertRaises(SystemExit) as ctx:
            docs.validate_manifest(sections, content_dir=self.content)
        self.assertIn("not listed in site.toml", str(ctx.exception))

    def test_sidebar_contains_every_section(self) -> None:
        site = docs.load_site(content_dir=self.content)
        html_out = docs.sidebar(site, "index")
        for section in site.sections:
            self.assertIn(section.title, html_out)

    def test_sidebar_grit_lib_api_links_to_library_guide(self) -> None:
        site = docs.load_site(content_dir=self.content)
        for current in ("index", "status", "library"):
            html_out = docs.sidebar(site, current)
            expected = docs.href_to(current, docs.LIBRARY_GUIDE_SLUG)
            self.assertIn(f'<a href="{expected}">grit-lib API</a>', html_out)

    def test_command_urls_unchanged(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            out = Path(tmp)
            docs.generate(out, content_dir=self.content)
            for name in ("status", "commit", "fetch", "upload-pack"):
                path = out / name / "index.html"
                self.assertTrue(path.is_file(), f"missing command page {name}")


if __name__ == "__main__":
    unittest.main()
