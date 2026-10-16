import importlib.util
import subprocess
import sys
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]


def _load_noncanonical_checker():
    path = ROOT / "scripts" / "check_noncanonical_grit_lib_imports.py"
    spec = importlib.util.spec_from_file_location("check_noncanonical_grit_lib_imports", path)
    assert spec and spec.loader
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


def _run_script(name: str) -> None:
    proc = subprocess.run(
        [sys.executable, str(ROOT / "scripts" / name)],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    assert proc.returncode == 0, proc.stderr or proc.stdout


class CanonicalLayoutScriptTests(unittest.TestCase):
    def test_canonical_layout_script_passes(self) -> None:
        _run_script("check_canonical_layout.py")

    def test_no_noncanonical_grit_lib_imports(self) -> None:
        _run_script("check_noncanonical_grit_lib_imports.py")

    def test_grouped_flat_import_detected_as_noncanonical(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = "use grit_lib::{odb::Odb, pack::PackFile};\n"
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        flats = {h[2] for h in hits}
        self.assertIn("odb", flats)
        self.assertIn("pack", flats)

    def test_grouped_canonical_import_allowed(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = "use grit_lib::{objects::odb::Odb, revwalk::rev_parse::resolve};\n"
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        self.assertEqual(hits, [])
