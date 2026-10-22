"""Unit tests for scripts/public_fn_coverage.py."""
from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "scripts"))

import public_fn_coverage as pfc  # noqa: E402


def _mini_rustdoc() -> dict:
    return {
        "includes_private": False,
        "index": {
            "1": {
                "name": "alpha",
                "visibility": "public",
                "inner": {"function": {"sig": {}, "generics": {}}},
            },
            "2": {
                "name": "hidden",
                "visibility": "private",
                "inner": {"function": {"sig": {}, "generics": {}}},
            },
        },
        "paths": {
            "1": {
                "crate_id": 0,
                "kind": "function",
                "path": ["grit_lib", "demo", "alpha"],
            },
            "2": {
                "crate_id": 0,
                "kind": "function",
                "path": ["grit_lib", "demo", "hidden"],
            },
            "3": {
                "crate_id": 1,
                "kind": "function",
                "path": ["std", "demo", "other"],
            },
        },
    }


def _mini_cov(*, demangled: str, count: int) -> dict:
    # Use a synthetic mangled name; tests patch demangle_names when needed.
    return {
        "data": [
            {
                "functions": [
                    {
                        "name": f"__TEST__{demangled}",
                        "count": count,
                        "filenames": ["/workspace/grit-lib/src/demo.rs"],
                        "regions": [],
                    }
                ]
            }
        ]
    }


class PublicFnCoverageTests(unittest.TestCase):
    def test_parse_public_functions_skips_private_and_other_crates(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "doc.json"
            path.write_text(json.dumps(_mini_rustdoc()), encoding="utf-8")
            fns = pfc.parse_public_functions(path)
            self.assertEqual([f.path for f in fns], ["grit_lib::demo::alpha"])

    def test_normalize_strips_generics_and_closures(self) -> None:
        raw = "grit_lib::foo::bar::{closure#0}::baz::<T, U>"
        self.assertEqual(
            pfc.normalize_rust_path(raw),
            "grit_lib::foo::bar::baz",
        )

    def test_normalize_inherent_impl_angle_brackets(self) -> None:
        raw = "<grit_lib::porcelain::rebase::RebaseTodoCmd>::as_str"
        self.assertEqual(
            pfc.normalize_rust_path(raw),
            "grit_lib::porcelain::rebase::RebaseTodoCmd::as_str",
        )

    def test_normalize_strips_monomorphized_impl_generics(self) -> None:
        raw = (
            "grit_lib::pkt_line::read_until_flush_or_delim::"
            "<core::io::cursor::Cursor<alloc::vec::Vec<u8>>>"
        )
        self.assertEqual(
            pfc.normalize_rust_path(raw),
            "grit_lib::pkt_line::read_until_flush_or_delim",
        )

    def test_check_fails_on_missing_and_stale_allowlist(self) -> None:
        public = [pfc.PublicFn(path="grit_lib::demo::alpha")]
        executions = {"grit_lib::demo::alpha": 0}
        _, failures = pfc.check(public, executions, [])
        self.assertTrue(any("zero executions" in f for f in failures))

        executions = {"grit_lib::demo::alpha": 3}
        _, failures = pfc.check(public, executions, ["grit_lib::demo::alpha"])
        self.assertTrue(any("now have coverage" in f for f in failures))

    def test_function_execution_counts_merges_json_files(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            left = Path(tmp) / "left.json"
            right = Path(tmp) / "right.json"
            left.write_text(
                json.dumps(_mini_cov(demangled="grit_lib::demo::alpha", count=1)),
                encoding="utf-8",
            )
            right.write_text(
                json.dumps(_mini_cov(demangled="grit_lib::demo::alpha", count=4)),
                encoding="utf-8",
            )

            def fake_demangle(names: list[str]) -> list[str]:
                return [n.removeprefix("__TEST__") for n in names]

            original = pfc.demangle_names
            pfc.demangle_names = fake_demangle  # type: ignore[method-assign]
            try:
                counts = pfc.function_execution_counts([left, right])
            finally:
                pfc.demangle_names = original  # type: ignore[method-assign]
            self.assertEqual(counts["grit_lib::demo::alpha"], 4)


if __name__ == "__main__":
    unittest.main()
