#!/usr/bin/env python3
"""Rewrite match arms from legacy Error object variants to Error::Object(ObjectError::...)."""

from __future__ import annotations

import re
from pathlib import Path

REPO = Path(__file__).resolve().parents[1]
SKIP = {REPO / "grit-lib/src/error.rs", REPO / "grit-lib/src/objects/error.rs"}

TUPLE = [
    "InvalidObjectId",
    "ObjectNotFound",
    "CorruptObject",
    "UnknownObjectType",
    "Zlib",
    "InvalidTreeEntry",
    "InvalidSignature",
    "Midx",
    "Offload",
    "TreeSha256Claim",
    "TreeSha256MissingSubmodules",
    "PromisedObjectMissing",
]

STRUCT = [
    "HashCollision",
    "PromisorFetchFailed",
    "PackIndexChecksumMismatch",
    "UnresolvedDeltas",
    "PackDeltaBaseNotWritten",
    "InvalidObject",
    "UnexpectedObjectKind",
    "CommitGraphParentMissing",
    "DeltaChainTooDeep",
    "ObjectHeaderTooLong",
    "CorruptLooseObject",
    "CorruptPackedObject",
    "LooseHashMismatch",
    "LooseObjectWrite",
    "ObjectStreamTruncated",
    "MaintenanceRunning",
    "NoPromisorRemote",
    "UnsupportedObjectStore",
    "PreciousObjectsRepository",
    "ObjectFormatMismatch",
    "OdbObjectsDirMismatch",
    "TreeSha256InvalidRevision",
    "TagVerifyNotTag",
]

UNIT = [
    "TreeSha256InvalidHex",
    "TreeSha256RequiresSigning",
    "TagVerifyMissingObjectHeader",
    "TagVerifyDuplicateTreeSha256Header",
]


def transform_line(line: str) -> str:
    if "Error::" not in line:
        return line
    for v in TUPLE:
        line = re.sub(
            rf"Error::{v}\(\_\)",
            f"Error::Object(ObjectError::{v}(_))",
            line,
        )
        line = re.sub(
            rf"Error::{v}\(\.\.\.\)",
            f"Error::Object(ObjectError::{v}(...))",
            line,
        )
        if v == "ObjectNotFound":
            line = re.sub(
                r"Error::ObjectNotFound\(([^)]+)\)",
                r"Error::Object(ObjectError::ObjectNotFound { spec: \1, .. })",
                line,
            )
        elif v == "InvalidObjectId":
            line = re.sub(
                r"Error::InvalidObjectId\(([^)]+)\)",
                r"Error::Object(ObjectError::InvalidObjectId { input: \1 })",
                line,
            )
        elif v == "CorruptObject":
            line = re.sub(
                r"Error::CorruptObject\(([^)]+)\)",
                r"Error::Object(ObjectError::CorruptObject { .. })",
                line,
            )
        elif v == "Zlib":
            line = re.sub(
                r"Error::Zlib\(([^)]+)\)",
                r"Error::Object(ObjectError::Zlib { .. })",
                line,
            )
        elif v == "UnknownObjectType":
            line = re.sub(
                r"Error::UnknownObjectType\(([^)]+)\)",
                r"Error::Object(ObjectError::UnknownObjectType { ty: \1 })",
                line,
            )
        elif v == "PromisedObjectMissing":
            line = re.sub(
                r"Error::PromisedObjectMissing\(([^)]+)\)",
                r"Error::Object(ObjectError::PromisedObjectMissing(\1))",
                line,
            )
        else:
            line = re.sub(
                rf"Error::{v}\(([^)]+)\)",
                rf"Error::Object(ObjectError::{v}(\1))",
                line,
            )
    for v in STRUCT:
        line = re.sub(
            rf"Error::{v}\s*\{{",
            f"Error::Object(ObjectError::{v} {{",
            line,
        )
        line = re.sub(
            rf"Error::{v}\(\.\.\.\)",
            f"Error::Object(ObjectError::{v} {{ .. }})",
            line,
        )
    for v in UNIT:
        line = re.sub(rf"\bError::{v}\b", f"Error::Object(ObjectError::{v})", line)
    # ObjectError bare in matches (wrong type)
    line = re.sub(
        r"\bObjectError::ObjectNotFound\(\_\)",
        "Error::Object(ObjectError::ObjectNotFound { .. })",
        line,
    )
    line = re.sub(
        r"\bObjectError::PromisedObjectMissing\(([^)]+)\)",
        r"Error::Object(ObjectError::PromisedObjectMissing(\1))",
        line,
    )
    return line


def main() -> None:
    for root in ["grit-lib", "grit-cli", "grit-examples", "grit-utils"]:
        p = REPO / root
        if not p.exists():
            continue
        for path in p.rglob("*.rs"):
            if path in SKIP:
                continue
            text = path.read_text(encoding="utf-8")
            lines = text.splitlines(keepends=True)
            new = [transform_line(ln) for ln in lines]
            out = "".join(new)
            if out != text:
                path.write_text(out)


if __name__ == "__main__":
    main()
