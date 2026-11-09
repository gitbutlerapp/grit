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

    def test_canonical_api_path_strips_lifetime(self) -> None:
        raw = "grit_lib::diff::Diff<'repo>::blame"
        self.assertEqual(
            am.canonical_api_path(raw),
            "grit_lib::diff::Diff::blame",
        )
        paths = {"grit_lib::diff::Diff<'repo>::blame"}
        self.assertTrue(
            am.replacement_exists("grit_lib::diff::Diff::blame", paths),
        )

    def test_annotated_grit_lib_path_is_validated(self) -> None:
        paths = {"grit_lib::merge::Merge::abort"}
        bogus = "grit_lib::definitely_missing::Nope (annotation)"
        self.assertFalse(am.replacement_exists(bogus, paths))
        bad = am.invalid_replacements(
            {"grit_lib::old::item": bogus},
            paths,
        )
        self.assertEqual(bad, [("grit_lib::old::item", bogus)])
        good = "grit_lib::merge::Merge::abort (rerere state)"
        self.assertTrue(am.replacement_exists(good, paths))

    def test_invalid_replacement_rejected(self) -> None:
        paths = {"grit_lib::repo_handles::Checkout::checkout"}
        self.assertFalse(
            am.replacement_exists(
                "grit_lib::repo_handles::Checkout::checkout_between_trees",
                paths,
            )
        )
        mapping = {
            "grit_lib::porcelain::checkout::checkout_between_trees": (
                "grit_lib::repo_handles::Checkout::checkout_between_trees"
            ),
        }
        bad = am.invalid_replacements(mapping, paths)
        self.assertEqual(len(bad), 1)

    def test_deprecated_note_after_attribute(self) -> None:
        body = [
            '/// Set `key` to `value` in the config file for `location`.',
            '#[deprecated(note = "use ConfigEditor on a repository config snapshot")]',
            "pub fn set_config_value(",
            "    location: ConfigLocation,",
            ")",
        ]
        note = am._deprecated_note_from_lines(body, 2)
        self.assertEqual(note, "use ConfigEditor on a repository config snapshot")

    def test_deprecation_notes_at_v0531(self) -> None:
        import subprocess

        try:
            subprocess.check_call(
                ["git", "rev-parse", "--verify", "v0.5.31"],
                cwd=ROOT,
                stdout=subprocess.DEVNULL,
                stderr=subprocess.DEVNULL,
            )
        except subprocess.CalledProcessError:
            self.skipTest("v0.5.31 tag not available")
        fn_notes = am._scan_deprecated_fn_notes("v0.5.31")
        self.assertIn("set_config_value", fn_notes)
        self.assertIn("ConfigEditor", fn_notes["set_config_value"])
        removed = ["grit_lib::config::set_config_value"]
        notes = am.deprecation_notes_at("v0.5.31", removed)
        self.assertIn("ConfigEditor", notes["grit_lib::config::set_config_value"])

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
