"""Verify TESTING.md upstream mapping rows reference real grit-lib tests."""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TESTING = ROOT / "TESTING.md"
TESTS_DIR = ROOT / "grit-lib" / "tests"
LIB_SRC = ROOT / "grit-lib" / "src"

TABLE_HEADER = "### Upstream test mapping"
ROW_RE = re.compile(r"^\| ([^|]+?) \|")
FN_RE = re.compile(r"`([a-zA-Z0-9_]+)`")

REQUIRED_UPSTREAM_IDS = (
    "t1006",
    "t1007",
    "t1050",
    "t1060",
    "t1450",
    "t5300",
    "t5302",
    "t5303",
    "t5308",
    "t5309",
    "t5313",
    "t5314",
    "t5316",
    "t5318",
    "t5319",
    "t5324",
    "t5325",
    "t5328",
    "t5334",
    "t5335",
    "t5351",
    "t5613",
    "t5615",
    "t5616",
)


def _mapping_section(text: str) -> str:
    start = text.index(TABLE_HEADER)
    rest = text[start:]
    end = rest.find("\n### ", len(TABLE_HEADER))
    if end == -1:
        end = rest.find("\n## ", len(TABLE_HEADER))
    return rest[:end] if end != -1 else rest


def _upstream_column(line: str) -> str | None:
    m = ROW_RE.match(line)
    if not m:
        return None
    return m.group(1).strip().strip("`")


def _rust_test_names() -> set[str]:
    """Function names that appear in a `#[test]` item (integration or unit tests)."""
    names: set[str] = set()
    test_fn = re.compile(
        r"#\[test\]\s*\n\s*fn\s+([a-zA-Z0-9_]+)\s*\(",
        re.MULTILINE,
    )
    for path in list(TESTS_DIR.glob("*.rs")) + list(TESTS_DIR.glob("**/*.rs")):
        body = path.read_text(encoding="utf-8")
        names.update(test_fn.findall(body))
    for path in LIB_SRC.rglob("*.rs"):
        body = path.read_text(encoding="utf-8")
        names.update(test_fn.findall(body))
    return names


class UpstreamMappingTests(unittest.TestCase):
    def test_required_upstream_ids_are_mapped(self) -> None:
        text = TESTING.read_text(encoding="utf-8")
        upstream_cols: list[str] = []
        for line in text.splitlines():
            if not line.startswith("|"):
                continue
            if (col := _upstream_column(line)) is not None and col.startswith("t"):
                upstream_cols.append(col)
        missing = [
            tid
            for tid in REQUIRED_UPSTREAM_IDS
            if not any(tid in col for col in upstream_cols)
        ]
        self.assertEqual([], missing, "TESTING.md missing upstream mapping rows")

    def test_mapping_rows_reference_existing_tests_or_skip(self) -> None:
        text = TESTING.read_text(encoding="utf-8")
        section = _mapping_section(text)
        rust_names = _rust_test_names()
        missing: list[str] = []
        for line in section.splitlines():
            if _upstream_column(line) is None:
                continue
            if "not applicable" in line.lower() or " skip" in line.lower():
                continue
            if "partial" in line.lower() and "(`" not in line:
                continue
            if re.search(r"`[^`]+\*`", line):
                continue
            if "fsck_objects.rs`" in line and "(`" not in line:
                continue
            if "`objects_parse_roundtrip.rs`" in line and "(`" not in line:
                continue
            parts = [p.strip() for p in line.split("|")]
            if len(parts) < 5:
                continue
            rust_col = parts[3]
            fns = [m for m in FN_RE.findall(rust_col) if not m.endswith(".rs")]
            if not fns:
                continue
            for fn in fns:
                if fn not in rust_names:
                    missing.append(f"{fn} ({line[:80]}...)")
        self.assertEqual([], missing, "unknown test functions in mapping table")


if __name__ == "__main__":
    unittest.main()
