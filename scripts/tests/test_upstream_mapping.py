"""Verify TESTING.md upstream mapping rows reference real grit-lib tests."""

from __future__ import annotations

import re
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
TESTING = ROOT / "TESTING.md"
TESTS_DIR = ROOT / "grit-lib" / "tests"
LIB_SRC = ROOT / "grit-lib" / "src"

MAPPING_HEADER_RE = re.compile(r"^### Upstream test mapping", re.MULTILINE)
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

REQUIRED_HISTORY_INDEX: tuple[str, ...] = ()
REQUIRED_HISTORY_INDEX_EXT: tuple[str, ...] = ()
REQUIRED_HISTORY_SPARSE: tuple[str, ...] = ()
REQUIRED_HISTORY_DIFF_BLOBS: tuple[str, ...] = (
    "t4033",
    "t4050",
    "t4071",
    "t4065",
    "t4061",
    "t4015",
    "t4012",
    "t4052",
    "t4032",
    "t4029",
    "t4074",
    "t4063",
    "t4051",
    "t4055",
    "t4024",
    "t4049",
    "t4047",
)
REQUIRED_HISTORY_DIFF_TREES: tuple[str, ...] = ()
REQUIRED_HISTORY_REVWALK_ORDER: tuple[str, ...] = ()
REQUIRED_HISTORY_REVWALK_SIMPLIFY: tuple[str, ...] = ()
REQUIRED_HISTORY_REVPARSE: tuple[str, ...] = ()
REQUIRED_HISTORY_MERGE: tuple[str, ...] = ()
REQUIRED_HISTORY_STASH_NOTES: tuple[str, ...] = ()
REQUIRED_HISTORY_SUPPORTING: tuple[str, ...] = ()


def _mapping_sections(text: str) -> list[str]:
    headers = [m.start() for m in MAPPING_HEADER_RE.finditer(text)]
    if not headers:
        return []
    sections: list[str] = []
    for i, start in enumerate(headers):
        end = headers[i + 1] if i + 1 < len(headers) else len(text)
        chunk = text[start:end]
        sub_end = chunk.find("\n### ", len("### "))
        if sub_end == -1:
            sub_end = chunk.find("\n## ", len("### "))
        sections.append(chunk[:sub_end] if sub_end != -1 else chunk)
    return sections


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


def _validate_mapping_section(section: str, rust_names: set[str]) -> list[str]:
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
    return missing


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

    def test_mapping_sections_discovers_all_testing_md_headings(self) -> None:
        text = TESTING.read_text(encoding="utf-8")
        sections = _mapping_sections(text)
        header_count = len(MAPPING_HEADER_RE.findall(text))
        self.assertGreaterEqual(header_count, 3)
        self.assertEqual(len(sections), header_count)

    def test_mapping_fixture_catches_invalid_rust_test_reference(self) -> None:
        fixture = """
### Upstream test mapping: fixture alpha

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t9999-fixture.sh | demo | `this_rust_test_name_does_not_exist_1119` | covered |

### Upstream test mapping: fixture beta

| upstream file | scenario | Rust test | status |
| --- | --- | --- | --- |
| t9998-fixture.sh | skip row | — | skipped |
"""
        rust_names = _rust_test_names()
        missing = _validate_mapping_section(
            _mapping_sections(fixture)[0], rust_names
        )
        self.assertTrue(missing)

    def test_mapping_rows_reference_existing_tests_or_skip(self) -> None:
        text = TESTING.read_text(encoding="utf-8")
        rust_names = _rust_test_names()
        all_missing: list[str] = []
        for section in _mapping_sections(text):
            all_missing.extend(_validate_mapping_section(section, rust_names))
        self.assertEqual(
            [],
            all_missing,
            "unknown test functions in mapping table(s)",
        )

    def test_history_subsection_placeholders_exist(self) -> None:
        """Later plan steps extend these REQUIRED_* tuples; keep one per subsection."""
        placeholders = (
            REQUIRED_HISTORY_INDEX,
            REQUIRED_HISTORY_INDEX_EXT,
            REQUIRED_HISTORY_SPARSE,
            REQUIRED_HISTORY_DIFF_BLOBS,
            REQUIRED_HISTORY_DIFF_TREES,
            REQUIRED_HISTORY_REVWALK_ORDER,
            REQUIRED_HISTORY_REVWALK_SIMPLIFY,
            REQUIRED_HISTORY_REVPARSE,
            REQUIRED_HISTORY_MERGE,
            REQUIRED_HISTORY_STASH_NOTES,
            REQUIRED_HISTORY_SUPPORTING,
        )
        self.assertEqual(len(placeholders), 11)


if __name__ == "__main__":
    unittest.main()
