"""Unit tests for grit-lib API map generation from rustdoc HTML."""
from __future__ import annotations

import shutil
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import apimap  # noqa: E402
import docs  # noqa: E402
import rustdoc_links  # noqa: E402


class ApimapTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        rustdoc_links.ensure_local_rustdoc(docs.DOC_ROOT, repo_root=ROOT)

    def test_api_map_lists_every_public_module(self) -> None:
        crate_dir = docs.DOC_ROOT / "grit_lib"
        expected = apimap.module_index_paths(crate_dir)
        rows, _ = apimap.build_rows(docs.DOC_ROOT)
        module_rows = [row for row in rows if row.kind == "module"]
        self.assertEqual(len(module_rows), len(expected))
        mapped = {row.qualified for row in module_rows}
        for index_path in expected:
            text = index_path.read_text(encoding="utf-8")
            title_match = __import__("re").search(
                r"<title>(grit_lib(?:::[A-Za-z0-9_]+)+) - Rust</title>",
                text,
            )
            self.assertIsNotNone(title_match, index_path)
            assert title_match is not None
            self.assertIn(title_match.group(1), mapped)

    def test_api_map_is_deterministic(self) -> None:
        first = apimap.api_map_markdown(docs.DOC_ROOT)
        second = apimap.api_map_markdown(docs.DOC_ROOT)
        self.assertEqual(first, second)
        lines = first.strip().splitlines()
        self.assertGreater(len(lines), 3)
        self.assertEqual(lines[0], "| Item | Kind | Summary | docs.rs |")

    def test_api_map_fails_on_module_without_summary(self) -> None:
        tmp = Path(tempfile.mkdtemp(prefix="grit-apimap-fixture-"))
        self.addCleanup(lambda: shutil.rmtree(tmp, ignore_errors=True))
        crate = tmp / "grit_lib"
        (crate / "nodoc").mkdir(parents=True)
        (crate / "index.html").write_text(
            '<a class="mod" href="nodoc/index.html" title="mod grit_lib::nodoc">nodoc</a>',
            encoding="utf-8",
        )
        (crate / "nodoc" / "index.html").write_text(
            "<title>grit_lib::nodoc - Rust</title><h1>Module nodoc</h1>",
            encoding="utf-8",
        )
        with self.assertRaises(SystemExit) as ctx:
            apimap.build_rows(tmp)
        msg = str(ctx.exception)
        self.assertIn("grit_lib::nodoc", msg)
        self.assertIn("summary", msg.lower())

    def test_llms_txt_includes_api_map_summary(self) -> None:
        with tempfile.TemporaryDirectory(prefix="grit-llms-apimap-") as tmp:
            tmp_path = Path(tmp)
            out = tmp_path / "docs"
            llms_dir = tmp_path / "llms"
            docs.generate(out, llms_dir=llms_dir)
            llms = (llms_dir / "llms.txt").read_text(encoding="utf-8")
            self.assertIn("library/api-map/index.md", llms)
            self.assertIn("Generated index of public grit-lib modules", llms)


if __name__ == "__main__":
    unittest.main()
