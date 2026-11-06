#!/usr/bin/env python3
"""Word-boundary migration of umbrella commit/merge Error variants (task 7107)."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

COMMIT_VARIANTS = [
    "EmptyCommitMessage",
    "CommitPathsRequired",
    "PartialCommitDuringMerge",
    "PartialCommitDuringReplay",
    "NothingToCommit",
    "NothingStagedToCommit",
    "AmendUnborn",
    "InvalidIdentity",
    "Identity",
]

MERGE_VARIANTS = [
    "NotFastForward",
    "UnrelatedHistories",
    "MergeCommit",
    "ReplaySourceAtHead",
    "SequencerUnknownCommand",
    "ReplayContinueUnmerged",
    "ReplayContinueDirtyIndex",
    "ReplaySkipAlreadyCommitted",
    "NoReplayInProgress",
    "NoMergeInProgress",
    "OperationInProgress",
]

SKIP_FILES = {
    "grit-lib/src/error.rs",
    "grit-lib/src/porcelain/commit_error.rs",
    "grit-lib/src/porcelain/merge_error.rs",
    "scripts/migrate_commit_merge_errors.py",
}


def replace_in_file(path: Path) -> bool:
    rel = path.relative_to(ROOT).as_posix()
    if rel in SKIP_FILES:
        return False
    text = path.read_text(encoding="utf-8")
    orig = text

    for var in COMMIT_VARIANTS:
        text = re.sub(rf"\bError::{var}\b", f"CommitError::{var}", text)

    for var in MERGE_VARIANTS:
        text = re.sub(rf"\bError::{var}\b", f"MergeError::{var}", text)

    # Signing(String) -> CommitError::Signing — handled separately in signing.rs

    if text != orig:
        path.write_text(text, encoding="utf-8")
        return True
    return False


def main() -> int:
    changed: list[str] = []
    for path in ROOT.rglob("*.rs"):
        if "target" in path.parts:
            continue
        if replace_in_file(path):
            changed.append(str(path.relative_to(ROOT)))
    for p in sorted(changed):
        print(p)
    print(f"updated {len(changed)} files", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
