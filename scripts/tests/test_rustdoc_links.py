"""Tests for rustdoc: link resolution in docs Markdown."""
from __future__ import annotations

import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import rustdoc_links  # noqa: E402


class RustdocLinksTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.doc_root = ROOT / "target" / "doc"
        if not (cls.doc_root / "grit_lib").is_dir():
            raise unittest.SkipTest("run cargo doc -p grit-lib --no-deps first")

    def test_resolve_repository_struct(self) -> None:
        path = rustdoc_links.resolve_local_rustdoc_html(
            self.doc_root, "grit_lib::repo::Repository"
        )
        self.assertEqual(path.name, "struct.Repository.html")

    def test_resolve_parse_tree_fn(self) -> None:
        path = rustdoc_links.resolve_local_rustdoc_html(
            self.doc_root, "grit_lib::objects::parse_tree"
        )
        self.assertEqual(path.name, "fn.parse_tree.html")

    def test_resolve_error_enum(self) -> None:
        path = rustdoc_links.resolve_local_rustdoc_html(
            self.doc_root, "grit_lib::error::Error"
        )
        self.assertEqual(path.name, "enum.Error.html")

    def test_expand_to_docs_rs(self) -> None:
        md = "See [`Repository`](rustdoc:grit_lib::repo::Repository)."
        out = rustdoc_links.expand_rustdoc_links(md, doc_root=self.doc_root)
        self.assertIn("https://docs.rs/grit-lib/latest/grit_lib/repo/struct.Repository.html", out)
        self.assertNotIn("rustdoc:", out)

    def test_expand_without_local_doc_fails(self) -> None:
        md = "[parse_tree](rustdoc:grit_lib::objects::parse_tree)"
        missing = self.doc_root.parent / "no-such-rustdoc-dir"
        with self.assertRaises(SystemExit):
            rustdoc_links.expand_rustdoc_links(md, doc_root=missing)

    def test_missing_item_raises(self) -> None:
        with self.assertRaises(FileNotFoundError):
            rustdoc_links.resolve_local_rustdoc_html(
                self.doc_root, "grit_lib::repo::NotARealType"
            )

    def test_validate_catches_bad_link_in_content(self) -> None:
        import tempfile

        with tempfile.TemporaryDirectory() as tmp:
            content = Path(tmp)
            (content / "bad.md").write_text(
                "[x](rustdoc:grit_lib::repo::NotARealType)\n", encoding="utf-8"
            )
            with self.assertRaises(SystemExit) as ctx:
                rustdoc_links.validate_rustdoc_links(content, self.doc_root)
            self.assertIn("NotARealType", str(ctx.exception))


if __name__ == "__main__":
    unittest.main()
