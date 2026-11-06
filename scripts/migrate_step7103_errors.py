#!/usr/bin/env python3
"""Mechanical migration for task 7103 ref/branch/notes error consolidation."""

from __future__ import annotations

import re
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]

SKIP = {
    "grit-lib/src/error.rs",
    "grit-lib/src/refs/error.rs",
    "grit-lib/src/notes/error.rs",
    "scripts/migrate_step7103_errors.py",
    "grit-lib/public-api.txt",
}

REPLACEMENTS: list[tuple[str, str]] = [
    # Head / ref state
    (r"\bError::DetachedHead\b", "Error::Ref(RefError::DetachedHead)"),
    (r"\bError::UnbornHead\b", "Error::Ref(RefError::UnbornHead)"),
    (r"\bError::InvalidHead\b", "Error::Ref(RefError::InvalidHead)"),
    # Ref store / lock / reftable wrappers
    (r"\bError::RefStore\(", "Error::Ref(RefError::Store("),
    (r"\bError::Reftable\(", "Error::Ref(RefError::Reftable("),
    (r"\bError::RefLock\(", "Error::Ref(RefError::RefLock("),
    # Branch umbrella -> BranchError
    (
        r"\bError::BranchNotFound\s*\{\s*branch:\s*",
        "Error::Branch(crate::porcelain::branch_error::BranchError::NotFound { name: ",
    ),
    (
        r"\bError::BranchCheckedOutInWorktree\s*\{\s*branch:\s*",
        "Error::Branch(crate::porcelain::branch_error::BranchError::CheckedOut { name: ",
    ),
    (
        r",\s*worktree_path:\s*",
        ", worktree: ",
    ),
    # Notes
    (r"\bError::InvalidNotesRef\(", "Error::Notes(NotesError::InvalidNotesRef { name: "),
    (r"\bError::NotesRewriteMode\(", "Error::Notes(NotesError::NotesRewriteMode { mode: "),
    # Tag verify
    (
        r"\bError::TagVerifyNotTag\s*\{",
        "Error::Tag(crate::porcelain::tag::TagError::VerifyNotTag {",
    ),
    # matches patterns
    (r"Error::InvalidRef\(_\)", "Error::Ref(RefError::InvalidName { .. })"),
    (r"Err\(Error::InvalidRef\(_\)\)", "Err(Error::Ref(RefError::InvalidName { .. }))"),
    (r"LibError::RefStore\(", "LibError::Ref(grit_lib::refs::RefError::Store("),
    (r"\bError::PackedRefsUnexpectedLine\b", "Error::Ref(RefError::PackedRefsUnexpectedLine"),
    (r"\bError::ReflogUnknownRef\b", "Error::Ref(RefError::ReflogUnknownRef"),
    (r"\bError::ReflogInvalidSelector\b", "Error::Ref(RefError::ReflogInvalidSelector"),
    (r"\bError::ReflogInvalidIndex\b", "Error::Ref(RefError::ReflogInvalidIndex"),
    (r"\bError::ReflogIndexOutOfRange\b", "Error::Ref(RefError::ReflogIndexOutOfRange"),
    (r"\bError::NoteAlreadyExists\b", "Error::Notes(NotesError::NoteAlreadyExists"),
    (r"\bError::NotesRefNotCommit\b", "Error::Notes(NotesError::NotesRefNotCommit"),
    (r"\bError::NotesObjectKind\b", "Error::Notes(NotesError::NotesObjectKind"),
    (r"\bError::NotesMergeBothEmpty\b", "Error::Notes(NotesError::NotesMergeBothEmpty"),
    (r"\bError::NotesMergeInProgress\b", "Error::Notes(NotesError::NotesMergeInProgress"),
]

INVALID_REF_RE = re.compile(r"\bError::InvalidRef\(")


def needs_ref_error_import(text: str) -> bool:
    return "RefError" in text and "RefError" not in text.split("fn ", 1)[0][:2000]


def add_imports(text: str, path: Path) -> str:
    rel = str(path.relative_to(ROOT))
    if "grit-cli" in rel:
        return text
    additions: list[str] = []
    if "RefError" in text and "use crate::refs::RefError" not in text:
        additions.append("use crate::refs::RefError;\n")
    if "NotesError" in text and "use crate::notes::NotesError" not in text:
        additions.append("use crate::notes::NotesError;\n")
    if not additions:
        return text
    # After last `use` block at top
    lines = text.splitlines(keepends=True)
    insert_at = 0
    for i, line in enumerate(lines):
        if line.startswith("use ") or line.startswith("pub use "):
            insert_at = i + 1
    return "".join(lines[:insert_at] + additions + lines[insert_at:])


def fix_invalid_notes_ref_closing(text: str) -> str:
    # Error::Notes(NotesError::InvalidNotesRef { name: expr })  — was InvalidNotesRef(expr)
    return text


def replace_invalid_ref(text: str) -> str:
    out = []
    i = 0
    while i < len(text):
        m = INVALID_REF_RE.search(text, i)
        if not m:
            out.append(text[i:])
            break
        out.append(text[i : m.start()])
        j = m.end()
        depth = 1
        while j < len(text) and depth:
            c = text[j]
            if c == "(":
                depth += 1
            elif c == ")":
                depth -= 1
            j += 1
        expr = text[m.end() : j - 1]
        out.append(f"Error::Ref(RefError::invalid_reason({expr}))")
        i = j
    return "".join(out)


def migrate_file(path: Path) -> bool:
    rel = str(path.relative_to(ROOT))
    if any(rel.startswith(s) or rel == s for s in SKIP):
        return False
    if path.suffix != ".rs":
        return False
    original = path.read_text()
    text = original
    for pat, repl in REPLACEMENTS:
        text = re.sub(pat, repl, text)
    text = replace_invalid_ref(text)
    # Notes: fix InvalidNotesRef / NotesRewriteMode closing braces
    text = re.sub(
        r"Error::Notes\(NotesError::InvalidNotesRef \{ name: ([^}]+)\)\)",
        r"Error::Notes(NotesError::InvalidNotesRef { name: \1 })",
        text,
    )
    text = re.sub(
        r"Error::Notes\(NotesError::NotesRewriteMode \{ mode: ([^}]+)\)\)",
        r"Error::Notes(NotesError::NotesRewriteMode { mode: \1 })",
        text,
    )
    # RefStore/RefLock/Reftable extra paren: Error::Ref(RefError::Store(x)) needs one more )
    # Store( takes RefStoreError - pattern Error::Ref(RefError::Store(foo) -> need Error::Ref(RefError::Store(foo))
    # The replacement already adds one paren level; original was Error::RefStore(e) -> Error::Ref(RefError::Store(e) - missing one )
    text = re.sub(
        r"Error::Ref\(RefError::Store\((RefStoreError::[^)]+(?:\([^)]*\))*)\)(?!\))",
        r"Error::Ref(RefError::Store(\1))",
        text,
    )
    text = add_imports(text, path)
    if text != original:
        path.write_text(text)
        return True
    return False


def main() -> int:
    changed = 0
    for path in ROOT.rglob("*.rs"):
        if "target/" in str(path):
            continue
        if migrate_file(path):
            changed += 1
            print(path.relative_to(ROOT))
    print(f"updated {changed} files")
    return 0


if __name__ == "__main__":
    sys.exit(main())
