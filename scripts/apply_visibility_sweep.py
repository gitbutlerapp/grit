#!/usr/bin/env python3
"""Apply pub(crate) to grit-lib public items listed by visibility_sweep_candidates.py."""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SRC = ROOT / "grit-lib" / "src"

# Items that must stay `pub` because a public `pub use` re-exports them elsewhere.
KEEP_PUBLIC: frozenset[str] = frozenset(
    {
        "AttrRule",
        "Parallelism",
        "MessageMatcher",
        "index_parallelism_from_config",
    }
)

# path suffix -> source file (flat implementation modules)
FLAT_MODULE_FILES: dict[str, Path] = {
    "reflog": SRC / "reflog.rs",
    "refspec": SRC / "refspec.rs",
    "reftable": SRC / "reftable.rs",
    "check_ref_format": SRC / "check_ref_format.rs",
    "ref_namespace": SRC / "ref_namespace.rs",
    "hide_refs": SRC / "hide_refs.rs",
    "ref_storage": SRC / "ref_storage.rs",
    "branch_tracking": SRC / "branch_tracking.rs",
    "refs_fsck": SRC / "refs_fsck.rs",
    "sparse_checkout": SRC / "sparse_checkout.rs",
    "split_index": SRC / "split_index.rs",
    "untracked_cache": SRC / "untracked_cache.rs",
    "resolve_undo": SRC / "resolve_undo.rs",
    "index_name_hash_lazy": SRC / "index_name_hash_lazy.rs",
    "write_tree": SRC / "write_tree.rs",
    "attributes": SRC / "attributes.rs",
    "crlf": SRC / "crlf.rs",
    "ignore": SRC / "ignore.rs",
    "pathspec": SRC / "pathspec.rs",
    "wildmatch": SRC / "wildmatch.rs",
    "worktree_cwd": SRC / "worktree_cwd.rs",
    "worktree_ref": SRC / "worktree_ref.rs",
    "worktree_rules": SRC / "worktree_rules.rs",
    "dotfile": SRC / "dotfile.rs",
    "environment": SRC / "environment.rs",
    "gitmodules": SRC / "gitmodules.rs",
    "ident_config": SRC / "ident_config.rs",
    "ident_resolve": SRC / "ident_resolve.rs",
    "init_filesystem": SRC / "init_filesystem.rs",
    "precompose_config": SRC / "precompose_config.rs",
    "git_date": SRC / "git_date.rs",
    "git_path": SRC / "git_path.rs",
    "mailmap": SRC / "mailmap.rs",
    "submodule_gitdir": SRC / "submodule_gitdir.rs",
}


def resolve_file(rust_path: str) -> Path | None:
    """Map grit_lib::a::b::item to a likely defining source file."""
    if not rust_path.startswith("grit_lib::"):
        return None
    parts = rust_path[len("grit_lib::") :].split("::")
    if not parts:
        return None
    if len(parts) == 1:
        return SRC / f"{parts[0]}.rs"
    if len(parts) == 2 and parts[0] in {
        "refs",
        "config",
        "worktree",
        "index",
        "state",
        "stash",
        "notes",
        "repo_handles",
        "command_runner",
        "tree_sha256",
        "path_validate",
        "lockfile",
        "line_range",
        "list_limit",
        "operation",
        "diagnostics",
        "gitbutler",
        "parallel",
    }:
        name = parts[1]
        p = SRC / parts[0] / f"{name}.rs"
        if p.is_file():
            return p
        return SRC / parts[0] / "mod.rs"
    if len(parts) >= 2 and parts[1] in FLAT_MODULE_FILES:
        return FLAT_MODULE_FILES[parts[1]]
    if parts[0] == "refs" and parts[1] == "store":
        if len(parts) == 2:
            return SRC / "refs" / "store" / "mod.rs"
        return SRC / "refs" / "store" / f"{parts[2]}.rs"
    if parts[0] in {"refs", "config", "worktree", "index"}:
        return SRC / parts[0] / "mod.rs"
    return SRC / f"{parts[0]}.rs"


def item_name(rust_path: str) -> str | None:
    parts = rust_path.split("::")
    last = parts[-1]
    if last and last[0].isupper():
        return last  # type / trait impl target
    if last.isidentifier():
        return last
    return None


def privatize_ident_in_file(path: Path, ident: str, kind: str) -> bool:
    text = path.read_text(encoding="utf-8")
    if kind == "fn":
        patterns = [
            (rf"^pub async fn {re.escape(ident)}\b", f"pub(crate) async fn {ident}"),
            (rf"^pub fn {re.escape(ident)}\b", f"pub(crate) fn {ident}"),
            (rf"^pub unsafe fn {re.escape(ident)}\b", f"pub(crate) unsafe fn {ident}"),
        ]
    elif kind == "struct":
        patterns = [(rf"^pub struct {re.escape(ident)}\b", f"pub(crate) struct {ident}")]
    elif kind == "enum":
        patterns = [(rf"^pub enum {re.escape(ident)}\b", f"pub(crate) enum {ident}")]
    elif kind == "trait":
        patterns = [(rf"^pub trait {re.escape(ident)}\b", f"pub(crate) trait {ident}")]
    elif kind == "type":
        patterns = [(rf"^pub type {re.escape(ident)}\b", f"pub(crate) type {ident}")]
    elif kind == "const":
        patterns = [(rf"^pub const {re.escape(ident)}\b", f"pub(crate) const {ident}")]
    else:
        return False
    new = text
    changed = False
    for pat, repl in patterns:
        updated, n = re.subn(pat, repl, new, count=1, flags=re.MULTILINE)
        if n:
            new = updated
            changed = True
            break
    if changed and new != text:
        path.write_text(new, encoding="utf-8")
    return changed


def classify_line(line: str) -> str | None:
    for kind in ("fn", "struct", "enum", "trait", "type", "const"):
        if f"pub {kind} " in line or f"pub async fn " in line:
            return kind if kind != "fn" or "async fn" not in line else "fn"
    if "pub async fn" in line:
        return "fn"
    return None


def main() -> None:
    p = argparse.ArgumentParser()
    p.add_argument("candidates_file", type=Path, help="Paths from visibility_sweep_candidates.py")
    p.add_argument("--dry-run", action="store_true")
    args = p.parse_args()
    paths = [
        ln.strip()
        for ln in args.candidates_file.read_text(encoding="utf-8").splitlines()
        if ln.strip() and not ln.startswith("#")
    ]
    applied = 0
    missed: list[str] = []
    for rust_path in paths:
        ident = item_name(rust_path)
        if ident is None or ident in KEEP_PUBLIC:
            continue
        # Skip trait impl methods (Type::method)
        if "::" in rust_path.split("grit_lib::", 1)[-1] and rust_path.split("::")[-2][0].isupper():
            continue
        file = resolve_file(rust_path)
        if file is None or not file.is_file():
            missed.append(rust_path)
            continue
        if args.dry_run:
            print(f"would privatize {ident} in {file.relative_to(ROOT)}")
            applied += 1
            continue
        if privatize_ident_in_file(file, ident, "fn") or privatize_ident_in_file(
            file, ident, "struct"
        ):
            applied += 1
        else:
            missed.append(rust_path)
    print(f"applied {applied}, missed {len(missed)}", file=sys.stderr)
    if missed and len(missed) <= 30:
        for m in missed:
            print(f"missed: {m}", file=sys.stderr)


if __name__ == "__main__":
    main()
