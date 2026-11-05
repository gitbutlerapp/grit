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

    def test_string_literal_noncanonical_path_allowed(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = 'assert!(path_violates("grit_lib::bench_fixture"));\n'
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        self.assertEqual(hits, [])

    def test_loop_label_does_not_hide_real_import(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = (
            "fn main() { 'retry: loop { let _ = grit_lib::bench_fixture::Fixture; "
            "break 'retry; } }\n"
        )
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        flats = {h[2] for h in hits}
        self.assertIn("bench_fixture", flats)

    def test_grouped_import_inside_string_literal_allowed(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = 'let doc = "use grit_lib::{odb::Odb};";\n'
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        self.assertEqual(hits, [])

    def test_scrub_preserves_lifetime_and_char_literal(self) -> None:
        chk = _load_noncanonical_checker()
        sample = "fn f(_: &'static str) { let _ = 'x'; }\n"
        scrubbed = chk.scrub_rust_literals(sample)
        self.assertIn("'static", scrubbed)
        self.assertNotIn("'x'", scrubbed)
        self.assertIn("  ", scrubbed)

    def test_scrub_raw_and_byte_strings(self) -> None:
        chk = _load_noncanonical_checker()
        sample = (
            'let a = br#"grit_lib::bench_fixture"#;\n'
            'let b = b"grit_lib::bench_fixture";\n'
        )
        scrubbed = chk.scrub_rust_literals(sample)
        self.assertNotIn("bench_fixture", scrubbed)

    def test_comment_quotes_do_not_hide_real_import(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = (
            '// opening quote: "\n'
            "let _ = grit_lib::bench_fixture::Fixture;\n"
            '// closing quote: "\n'
        )
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        flats = {h[2] for h in hits}
        self.assertIn("bench_fixture", flats)
        self.assertEqual(hits[0][1], 2)

    def test_string_in_code_still_scrubbed_after_comment_handling(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = 'let s = "grit_lib::bench_fixture";\n'
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        self.assertEqual(hits, [])

    def test_block_comment_does_not_start_string(self) -> None:
        chk = _load_noncanonical_checker()
        mapping = chk.parse_map()
        sample = "/* quote: \" */\nlet _ = grit_lib::bench_fixture::X;\n"
        hits = chk.find_violations_in_text("sample.rs", sample, mapping)
        flats = {h[2] for h in hits}
        self.assertIn("bench_fixture", flats)
