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
import rustdoc_links  # noqa: E402


class DocsManifestTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        rustdoc_links.ensure_local_rustdoc(docs.DOC_ROOT, repo_root=ROOT)

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

    def test_rustdoc_links_resolve_function_and_enum_kinds(self) -> None:
        fn_md = docs.prepare_markdown_body("[f](rustdoc:grit_lib::objects::parse_tree)")
        self.assertIn("grit_lib/objects/fn.parse_tree.html", fn_md)
        self.assertNotIn("struct.parse_tree", fn_md)
        self.assertNotIn("grit_lib/grit_lib", fn_md)

        enum_md = docs.prepare_markdown_body("[e](rustdoc:grit_lib::error::Error)")
        self.assertIn("grit_lib/error/enum.Error.html", enum_md)

    def test_generate_builds_rustdoc_when_grit_lib_docs_missing(self) -> None:
        doc_root = docs.DOC_ROOT
        grit_lib = doc_root / "grit_lib"
        self.assertTrue(grit_lib.is_dir(), "setUpClass should have built rustdoc")

        backup_parent = Path(tempfile.mkdtemp(prefix="grit-rustdoc-backup-"))
        self.addCleanup(lambda: shutil.rmtree(backup_parent, ignore_errors=True))
        shutil.move(str(grit_lib), str(backup_parent / "grit_lib"))

        def restore() -> None:
            if not grit_lib.is_dir() and (backup_parent / "grit_lib").is_dir():
                shutil.move(str(backup_parent / "grit_lib"), str(grit_lib))

        self.addCleanup(restore)

        with tempfile.TemporaryDirectory(prefix="grit-docs-out-") as tmp:
            docs.generate(Path(tmp), content_dir=self.content)
            html = (Path(tmp) / "library" / "objects" / "index.html").read_text(encoding="utf-8")
            self.assertIn("grit_lib/objects/fn.parse_tree.html", html)
            self.assertNotIn("struct.parse_tree", html)

        restore()

    def test_missing_include_file_fails(self) -> None:
        page = self.content / "library-quickstart.md"
        text = page.read_text(encoding="utf-8")
        page.write_text(
            text.replace(
                "grit-examples/src/bin/quickstart.rs",
                "grit-examples/src/bin/does-not-exist.rs",
            ),
            encoding="utf-8",
        )
        with self.assertRaises(SystemExit) as ctx:
            docs.load_site(content_dir=self.content)
        self.assertIn("include missing file", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
