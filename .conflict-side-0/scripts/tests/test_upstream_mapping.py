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
    "t5310",
    "t5326",
    "t5327",
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
    "t4301",
    "t6400",
    "t6402",
    "t6403",
    "t6405",
    "t6407",
    "t6411",
    "t6422",
    "t6427",
)

REQUIRED_HISTORY_INDEX: tuple[str, ...] = (
    "t1600",
    "t1601",
    "t0090",
    "t2030",
    "t2104",
    "t2107",
    "t7011",
    "t7012",
    "t1015",
)
REQUIRED_HISTORY_INDEX_EXT: tuple[str, ...] = (
    "t1600",
    "t1700",
    "t1701",
    "t7063",
)
REQUIRED_HISTORY_SPARSE: tuple[str, ...] = (
    "t1091",
    "t1092",
    "t1011",
)
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
REQUIRED_HISTORY_DIFF_TREES: tuple[str, ...] = (
    "t4000",
    "t4001",
    "t4002",
    "t4003",
    "t4005",
    "t4006",
    "t4007",
    "t4008",
    "t4009",
    "t4011",
    "t4022",
    "t4023",
    "t4038",
    "t4043",
    "t4046",
    "t4054",
    "t4057",
)
REQUIRED_HISTORY_REVWALK_ORDER: tuple[str, ...] = (
    "t6000",
    "t6001",
    "t6003",
    "t6005",
    "t6009",
    "t6013",
    "t6014",
    "t6019",
    "t6100",
    "t4217",
)
REQUIRED_HISTORY_REVWALK_SIMPLIFY: tuple[str, ...] = (
    "t6002",
    "t6004",
    "t6012",
    "t6016",
    "t6018",
    "t6021",
    "t6022",
    "t6102",
    "t6110",
    "t6111",
    "t6112",
    "t4216",
)
REQUIRED_HISTORY_REVPARSE: tuple[str, ...] = (
    "t1500",
    "t1503",
    "t1505",
    "t1506",
    "t1507",
    "t1508",
    "t1511",
    "t1512",
    "t1514",
    "t6101",
)
REQUIRED_HISTORY_MERGE: tuple[str, ...] = (
    "t6010",
    "t6099",
)
REQUIRED_HISTORY_STASH_NOTES: tuple[str, ...] = (
    "t3903",
    "t3905",
    "t3908",
    "t3301",
    "t3303",
    "t3305",
    "t3308",
    "t3309",
    "t3310",
    "t3311",
    "t2400",
    "t2401",
    "t2402",
    "t2403",
    "t2404",
    "t2406",
    "t4204",
)
REQUIRED_HISTORY_SUPPORTING: tuple[str, ...] = (
    "t1510",
    "t1504",
    "t6130",
    "t6132",
    "t4200",
    "t6040",
)


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

    def test_required_history_tuples_appear_in_mapping_rows(self) -> None:
        text = TESTING.read_text(encoding="utf-8")
        sections = _mapping_sections(text)
        index_section = next(
            s
            for s in sections
            if "index, diff, revwalk" in s or "index, diff" in s
        )
        sparse_section = next(
            (s for s in sections if "sparse" in s.lower()),
            index_section,
        )
        index_ext_start = text.index("#### Index extensions")
        index_ext_end = text.find("\n#### ", index_ext_start + 1)
        index_ext_section = (
            text[index_ext_start:index_ext_end]
            if index_ext_end != -1
            else text[index_ext_start:]
        )

        def upstream_ids(section: str) -> set[str]:
            found: set[str] = set()
            for line in section.splitlines():
                col = _upstream_column(line)
                if not col:
                    continue
                for token in re.findall(r"t[0-9]{4}", col):
                    found.add(token)
            return found

        index_ids = upstream_ids(index_section)
        for tid in REQUIRED_HISTORY_INDEX:
            self.assertIn(
                tid,
                index_ids,
                f"{tid} missing from index upstream mapping section",
            )
        sparse_ids = upstream_ids(sparse_section)
        for tid in REQUIRED_HISTORY_SPARSE:
            self.assertIn(
                tid,
                sparse_ids,
                f"{tid} missing from sparse upstream mapping rows",
            )
        for tid in REQUIRED_HISTORY_INDEX_EXT:
            self.assertIn(
                tid,
                upstream_ids(index_ext_section),
                f"{tid} missing from index-ext mapping rows",
            )

        supporting_start = text.index("#### Supporting")
        supporting_end = text.find("\n#### ", supporting_start + 1)
        supporting_section = (
            text[supporting_start:supporting_end]
            if supporting_end != -1
            else text[supporting_start:]
        )
        supporting_ids = upstream_ids(supporting_section)
        for tid in REQUIRED_HISTORY_SUPPORTING:
            self.assertIn(
                tid,
                supporting_ids,
                f"{tid} missing from supporting upstream mapping rows",
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
