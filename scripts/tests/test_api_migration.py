"""Unit tests for scripts/api_migration.py."""
from __future__ import annotations

import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import api_migration as am  # noqa: E402


SAMPLE_OLD = """
pub fn grit_lib::foo::bar() -> ()
pub fn grit_lib::repo::gone() -> ()
"""

SAMPLE_NEW = """
pub fn grit_lib::foo::bar() -> ()
pub fn grit_lib::repo::new_home() -> ()
"""


class ApiMigrationTests(unittest.TestCase):
    def test_paths_in_snapshot_finds_removed(self) -> None:
        old = am.paths_in_snapshot(SAMPLE_OLD)
        new = am.paths_in_snapshot(SAMPLE_NEW)
        self.assertIn("grit_lib::repo::gone", old - new)
        self.assertNotIn("grit_lib::foo::bar", old - new)

    def test_is_migration_path_rejects_signature_fragments(self) -> None:
        self.assertFalse(am._is_migration_path("grit_lib::error::Result<Foo"))
        self.assertTrue(am._is_migration_path("grit_lib::config::set_config_value"))

    def test_load_map_round_trip(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "map.toml"
            path.write_text(
                '# test\n"grit_lib::old" = "grit_lib::new"\n',
                encoding="utf-8",
            )
            self.assertEqual(am.load_map(path), {"grit_lib::old": "grit_lib::new"})

    def test_render_table_marks_unmapped(self) -> None:
        table = am.render_table({"grit_lib::gone": "removed, no replacement"}, {})
        self.assertIn("—", table)
        self.assertIn("grit_lib::gone", table)

    def test_check_fails_on_unmapped(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            doc = Path(tmp) / "migrating.md"
            doc.write_text(
                "intro\n\n<!-- api-migration:begin -->\n\nold\n\n<!-- api-migration:end -->\n",
                encoding="utf-8",
            )
            map_path = Path(tmp) / "map.toml"
            map_path.write_text('"grit_lib::missing" = "grit_lib::new"\n', encoding="utf-8")
            original_doc = am.DOC_PATH
            original_map = am.MAP_PATH
            try:
                am.DOC_PATH = doc
                am.MAP_PATH = map_path

                def fake_removed() -> list[str]:
                    return ["grit_lib::missing", "grit_lib::unmapped"]

                original_fn = am.removed_paths
                am.removed_paths = fake_removed  # type: ignore[assignment]
                try:
                    ok, msg = am.update_doc(check=True)
                finally:
                    am.removed_paths = original_fn  # type: ignore[assignment]
                self.assertFalse(ok)
                self.assertIn("unmapped", msg)
            finally:
                am.DOC_PATH = original_doc
                am.MAP_PATH = original_map


if __name__ == "__main__":
    unittest.main()
