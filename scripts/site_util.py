"""Shared helpers for static site generation and CI freshness checks."""
from __future__ import annotations

import filecmp
from pathlib import Path


def collect_files(root: Path) -> dict[Path, Path]:
    """Map relative paths to absolute file paths under ``root``."""
    if not root.is_dir():
        return {}
    return {
        path.relative_to(root): path
        for path in sorted(root.rglob("*"))
        if path.is_file()
    }


def compare_directories(generated: Path, committed: Path, *, label: str) -> list[str]:
    """Return human-readable diff lines when ``committed`` does not match ``generated``."""
    gen_map = collect_files(generated)
    com_map = collect_files(committed)
    issues: list[str] = []

    only_gen = sorted(gen_map.keys() - com_map.keys())
    only_com = sorted(com_map.keys() - gen_map.keys())
    for rel in only_gen:
        issues.append(f"{label}: extra generated file {rel.as_posix()}")
    for rel in only_com:
        issues.append(f"{label}: missing generated file {rel.as_posix()} (removed from output?)")

    for rel in sorted(gen_map.keys() & com_map.keys()):
        if not filecmp.cmp(gen_map[rel], com_map[rel], shallow=False):
            issues.append(f"{label}: content differs for {rel.as_posix()}")

    return issues
