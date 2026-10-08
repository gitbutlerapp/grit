"""Shared helpers for static site generation and CI freshness checks."""
from __future__ import annotations

import filecmp
import re
from collections.abc import Callable
from datetime import date
from pathlib import Path

MARKDOWN_LINK_RE = re.compile(r"\[([^\]]*)\]\(([^)]+)\)")


def compose_markdown_twin(
    title: str,
    body: str,
    *,
    summary: str = "",
    published: date | None = None,
) -> str:
    """Build a Markdown twin document with title, optional date and summary, then body."""
    parts = [f"# {title}"]
    if published is not None:
        parts.append(f"**Date:** {published.isoformat()}")
    if summary.strip():
        parts.append(f"> {summary.strip()}")
    parts.append(body.rstrip())
    return "\n\n".join(parts) + "\n"


def rewrite_markdown_links(body: str, url_for_path: Callable[[str], str | None]) -> str:
    """Rewrite ``[text](path)`` links when ``url_for_path`` returns an absolute URL."""

    def replace(match: re.Match[str]) -> str:
        text, url = match.group(1), match.group(2).strip()
        fragment = ""
        path = url
        if "#" in url:
            path, frag = url.split("#", 1)
            fragment = f"#{frag}" if frag else ""
        if not path:
            return match.group(0)
        lowered = path.lower()
        if lowered.startswith(("http://", "https://", "mailto:", "tel:")):
            absolute = url_for_path(path)
            if absolute is None:
                return match.group(0)
            return f"[{text}]({absolute}{fragment})"
        if path.startswith("#"):
            return match.group(0)
        absolute = url_for_path(path)
        if absolute is None:
            return match.group(0)
        return f"[{text}]({absolute}{fragment})"

    return MARKDOWN_LINK_RE.sub(replace, body)


def file_equals(left: Path, right: Path) -> bool:
    """Return whether two files have identical contents."""
    return left.is_file() and right.is_file() and filecmp.cmp(left, right, shallow=False)


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
