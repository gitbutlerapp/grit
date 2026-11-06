#!/usr/bin/env python3
"""Replace legacy Error object-store variants with ObjectError::....into()."""

from __future__ import annotations

import sys
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SKIP = {
    REPO / "grit-lib/src/error.rs",
    REPO / "grit-lib/src/objects/error.rs",
}

ALL = [
    "InvalidObjectId",
    "HashCollision",
    "ObjectNotFound",
    "PromisedObjectMissing",
    "PromisorFetchFailed",
    "CorruptObject",
    "PackIndexChecksumMismatch",
    "UnresolvedDeltas",
    "PackDeltaBaseNotWritten",
    "InvalidObject",
    "InvalidTreeEntry",
    "InvalidSignature",
    "UnexpectedObjectKind",
    "CommitGraphParentMissing",
    "DeltaChainTooDeep",
    "UnknownObjectType",
    "ObjectHeaderTooLong",
    "Zlib",
    "CorruptLooseObject",
    "CorruptPackedObject",
    "LooseHashMismatch",
    "LooseObjectWrite",
    "ObjectStreamTruncated",
    "MaintenanceRunning",
    "Midx",
    "Offload",
    "NoPromisorRemote",
    "UnsupportedObjectStore",
    "PreciousObjectsRepository",
    "ObjectFormatMismatch",
    "OdbObjectsDirMismatch",
    "TreeSha256InvalidHex",
    "TreeSha256InvalidRevision",
    "TreeSha256MissingSubmodules",
    "TreeSha256RequiresSigning",
    "TagVerifyNotTag",
    "TagVerifyMissingObjectHeader",
    "TagVerifyDuplicateTreeSha256Header",
    "TreeSha256Claim",
]

UNIT = {
    "TreeSha256InvalidHex",
    "TreeSha256RequiresSigning",
    "TagVerifyMissingObjectHeader",
    "TagVerifyDuplicateTreeSha256Header",
}


def find_matching(s: str, open_idx: int, open_c: str, close_c: str) -> int:
    depth = 0
    i = open_idx
    while i < len(s):
        c = s[i]
        if c in "\"'":
            q = c
            i += 1
            while i < len(s):
                if s[i] == "\\":
                    i += 2
                    continue
                if s[i] == q:
                    break
                i += 1
        elif c == open_c:
            depth += 1
        elif c == close_c:
            depth -= 1
            if depth == 0:
                return i
        i += 1
    raise ValueError("unbalanced")


def expr(variant: str, payload: str) -> str:
    if variant == "InvalidObjectId":
        return f"ObjectError::InvalidObjectId {{ input: {payload} }}"
    if variant == "ObjectNotFound":
        return f"ObjectError::object_not_found({payload})"
    if variant == "CorruptObject":
        return f"ObjectError::corrupt_object(None, {payload})"
    if variant == "Zlib":
        return f"ObjectError::Zlib {{ detail: {payload} }}"
    if variant == "UnknownObjectType":
        return f"ObjectError::UnknownObjectType {{ ty: {payload} }}"
    if variant in UNIT:
        return f"ObjectError::{variant}"
    if payload.startswith("{") and payload.endswith("}"):
        return f"ObjectError::{variant} {payload}"
    if payload:
        return f"ObjectError::{variant}({payload})"
    return f"ObjectError::{variant}"


def should_skip_line(line: str) -> bool:
    stripped = line.lstrip()
    if stripped.startswith("//"):
        return True
    if "=>" in line and "Error::" in line:
        return True
    if "matches!(" in line and "Error::" in line:
        return True
    if stripped.startswith("let Error::"):
        return True
    return False


def transform_line(line: str) -> str:
    if should_skip_line(line):
        return line
    out = []
    i = 0
    while i < len(line):
        found = None
        for variant in ALL:
            needle = f"Error::{variant}"
            j = line.find(needle, i)
            if j == -1:
                continue
            k = j + len(needle)
            if k < len(line) and line[k : k + 2] == "::":
                continue
            if found is None or j < found[0]:
                found = (j, variant, k)
        if found is None:
            out.append(line[i:])
            break
        j, variant, k = found
        if j >= 6 and line[j - 6 : j] == "Object":
            out.append(line[i : j + 6])
            i = j + 6
            continue
        out.append(line[i:j])
        if variant in UNIT and (k >= len(line) or line[k] not in "({"):
            out.append(expr(variant, "") + ".into()")
            i = k
            continue
        if k < len(line) and line[k] == "(":
            try:
                close = find_matching(line, k, "(", ")")
            except ValueError:
                out.append(needle)
                i = k
                continue
            payload = line[k + 1 : close]
            out.append(expr(variant, payload) + ".into()")
            i = close + 1
        elif k < len(line) and line[k] == "{":
            try:
                close = find_matching(line, k, "{", "}")
            except ValueError:
                out.append(needle)
                i = k
                continue
            payload = line[k : close + 1]
            out.append(expr(variant, payload) + ".into()")
            i = close + 1
        else:
            out.append(f"Error::{variant}")
            i = k
    return "".join(out)


def ensure_import(text: str, path: Path) -> str:
    if "ObjectError::" not in text:
        return text
    rel = str(path.relative_to(REPO))
    if rel.startswith("grit-lib/") and "use crate::objects::ObjectError" not in text:
        # after module docs
        lines = text.splitlines(keepends=True)
        i = 0
        while i < len(lines) and (lines[i].startswith("//!") or lines[i].strip() == ""):
            i += 1
        lines.insert(i, "use crate::objects::ObjectError;\n")
        return "".join(lines)
    if rel.startswith("grit-cli/") and "use grit_lib::objects::ObjectError" not in text:
        lines = text.splitlines(keepends=True)
        i = 0
        while i < len(lines) and (lines[i].startswith("//!") or lines[i].strip() == ""):
            i += 1
        lines.insert(i, "use grit_lib::objects::ObjectError;\n")
        return "".join(lines)
    if rel.startswith("grit-examples/") and "use grit_lib::objects::ObjectError" not in text:
        lines = text.splitlines(keepends=True)
        i = 0
        while i < len(lines) and (lines[i].startswith("//!") or lines[i].strip() == ""):
            i += 1
        lines.insert(i, "use grit_lib::objects::ObjectError;\n")
        return "".join(lines)
    return text


def transform_file(path: Path) -> bool:
    text = path.read_text(encoding="utf-8")
    lines = text.splitlines(keepends=True)
    new_lines = [transform_line(ln) for ln in lines]
    new = ensure_import("".join(new_lines), path)
    if new != text:
        path.write_text(new, encoding="utf-8")
        return True
    return False


def main() -> int:
    changed = 0
    for root in ["grit-lib", "grit-cli", "grit-examples", "grit-utils", "grit-test-support"]:
        p = REPO / root
        if not p.exists():
            continue
        for path in p.rglob("*.rs"):
            if path in SKIP:
                continue
            if transform_file(path):
                changed += 1
    print(f"updated {changed} files")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
