#!/usr/bin/env python3
"""Mechanical helpers for ObjectError migration (task 7102). Run from repo root."""

from __future__ import annotations

import re
import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
GRIT_LIB = REPO / "grit-lib"

# Simple Error::Variant(...) -> ObjectError::... .into() for Err(...) and return Err(...)
REPLACEMENTS: list[tuple[str, str]] = [
    (
        r"Err\(Error::InvalidObjectId\(([^)]+)\)\)",
        r"Err(ObjectError::InvalidObjectId { input: \1 }.into())",
    ),
    (
        r"return Err\(Error::InvalidObjectId\(([^)]+)\)\)",
        r"return Err(ObjectError::InvalidObjectId { input: \1 }.into())",
    ),
    (
        r"Error::InvalidObjectId\(([^)]+)\)",
        r"ObjectError::InvalidObjectId { input: \1 }.into()",
    ),
    (
        r"Err\(Error::ObjectNotFound\(([^)]+)\)\)",
        r"Err(ObjectError::object_not_found(\1).into())",
    ),
    (
        r"return Err\(Error::ObjectNotFound\(([^)]+)\)\)",
        r"return Err(ObjectError::object_not_found(\1).into())",
    ),
    (
        r"Error::ObjectNotFound\(([^)]+)\)",
        r"ObjectError::object_not_found(\1).into()",
    ),
    (
        r"Err\(Error::Zlib\(([^)]+)\)\)",
        r"Err(ObjectError::Zlib { detail: \1 }.into())",
    ),
    (
        r"Error::Zlib\(([^)]+)\)",
        r"ObjectError::Zlib { detail: \1 }.into()",
    ),
    (
        r"matches!\([^,]+,\s*Error::ObjectNotFound\(_\)",
        r"matches!(\1, Error::Object(ObjectError::ObjectNotFound { .. }))",
    ),
]

SCOPE_DIRS = [
    GRIT_LIB / "src" / "objects",
    GRIT_LIB / "src" / "odb",
    GRIT_LIB / "src" / "pack.rs",
    GRIT_LIB / "src" / "midx.rs",
    GRIT_LIB / "src" / "index_pack.rs",
    GRIT_LIB / "src" / "pack_objects.rs",
    GRIT_LIB / "src" / "pack_write.rs",
    GRIT_LIB / "src" / "pack_index.rs",
    GRIT_LIB / "src" / "pack_receive.rs",
    GRIT_LIB / "src" / "promisor_fetch.rs",
    GRIT_LIB / "src" / "maintenance.rs",
    GRIT_LIB / "src" / "local_object_copy.rs",
    GRIT_LIB / "src" / "commit_graph_file.rs",
    GRIT_LIB / "src" / "commit_graph_write.rs",
    GRIT_LIB / "src" / "fsck",
    GRIT_LIB / "src" / "unpack_objects.rs",
    GRIT_LIB / "src" / "zlib_inflate.rs",
]


def count_error_message(paths: list[Path]) -> int:
    total = 0
    for path in paths:
        if path.is_file():
            total += path.read_text(encoding="utf-8").count("Error::Message(")
        elif path.is_dir():
            for f in path.rglob("*.rs"):
                total += f.read_text(encoding="utf-8").count("Error::Message(")
    return total


def main() -> int:
    if len(sys.argv) < 2:
        print("usage: migrate_object_error.py count-message | apply-simple")
        return 2
    cmd = sys.argv[1]
    if cmd == "count-message":
        print(count_error_message(SCOPE_DIRS))
        return 0
    if cmd == "apply-simple":
        changed = 0
        files: set[Path] = set()
        for scope in SCOPE_DIRS:
            if scope.is_file():
                files.add(scope)
            else:
                files.update(scope.rglob("*.rs"))
        for path in sorted(files):
            text = path.read_text(encoding="utf-8")
            orig = text
            for pattern, repl in REPLACEMENTS:
                text = re.sub(pattern, repl, text)
            if text != orig:
                path.write_text(text, encoding="utf-8")
                changed += 1
        print(f"updated {changed} files")
        return 0
    print(f"unknown command {cmd}")
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
