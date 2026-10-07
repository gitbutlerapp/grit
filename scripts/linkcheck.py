#!/usr/bin/env python3
"""Verify internal links in the static site under docs/."""
from __future__ import annotations

import argparse
import re
import sys
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SITE_ROOT = ROOT / "docs"

HREF_RE = re.compile(r"""(?:href|src)\s*=\s*["']([^"']+)["']""", re.IGNORECASE)
ID_RE = re.compile(r"""\bid\s*=\s*["']([^"']+)["']""")


@dataclass(frozen=True)
class LinkRef:
    source: Path
    raw: str
    line: int


@dataclass(frozen=True)
class LinkIssue:
    source: Path
    raw: str
    detail: str
    line: int = 0


def is_skipped_scheme(url: str) -> bool:
    lowered = url.lower()
    return lowered.startswith(("mailto:", "tel:", "javascript:", "data:"))


def is_external(url: str) -> bool:
    return url.startswith(("http://", "https://", "//"))


def normalize_external_url(url: str) -> str:
    """Return an absolute URL suitable for ``urllib.request``."""
    if url.startswith("//"):
        return f"https:{url}"
    if url.startswith(("http://", "https://")):
        return url
    raise ValueError(f"unsupported external URL scheme: {url!r}")


def page_ids(html: str) -> set[str]:
    return set(ID_RE.findall(html))


def load_page_ids(path: Path, cache: dict[Path, set[str]]) -> set[str]:
    if path not in cache:
        cache[path] = page_ids(path.read_text(encoding="utf-8"))
    return cache[path]


def resolve_target(source: Path, url: str) -> tuple[Path | None, str | None]:
    """Resolve ``url`` relative to ``source``; return (file path, fragment)."""
    fragment: str | None = None
    if "#" in url:
        url, frag = url.split("#", 1)
        fragment = frag or None
    if not url:
        return source, fragment

    if url.startswith("/"):
        base = SITE_ROOT
        path_part = url.lstrip("/")
    else:
        base = source.parent
        path_part = url

    candidate = (base / path_part).resolve()
    try:
        candidate.relative_to(SITE_ROOT.resolve())
    except ValueError:
        return None, fragment

    if candidate.is_dir():
        candidate = candidate / "index.html"
    elif candidate.suffix == "":
        index = candidate / "index.html"
        if index.is_file():
            candidate = index
        elif not candidate.is_file():
            candidate = candidate.with_suffix(".html")

    if candidate.is_file():
        return candidate, fragment
    return None, fragment


def iter_html_files() -> list[Path]:
    return sorted(SITE_ROOT.rglob("*.html"))


def collect_links(path: Path) -> list[LinkRef]:
    refs: list[LinkRef] = []
    for line_no, line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        for match in HREF_RE.finditer(line):
            refs.append(LinkRef(path, match.group(1), line_no))
    return refs


def check_link(
    ref: LinkRef,
    *,
    check_external: bool,
    id_cache: dict[Path, set[str]],
) -> LinkIssue | None:
    url = ref.raw.strip()
    if not url or is_skipped_scheme(url):
        return None
    if is_external(url):
        if not check_external:
            return None
        try:
            fetch_url = normalize_external_url(url)
        except ValueError as exc:
            return LinkIssue(ref.source, ref.raw, str(exc), ref.line)
        request = urllib.request.Request(fetch_url, method="HEAD")
        try:
            with urllib.request.urlopen(request, timeout=15) as response:
                if response.status >= 400:
                    return LinkIssue(ref.source, ref.raw, f"HTTP {response.status}", ref.line)
        except urllib.error.HTTPError as exc:
            return LinkIssue(ref.source, ref.raw, f"HTTP {exc.code}", ref.line)
        except urllib.error.URLError as exc:
            return LinkIssue(ref.source, ref.raw, str(exc.reason), ref.line)
        except ValueError as exc:
            return LinkIssue(ref.source, ref.raw, str(exc), ref.line)
        return None

    target, fragment = resolve_target(ref.source, url)
    if target is None:
        return LinkIssue(ref.source, ref.raw, "target file not found", ref.line)

    if fragment is not None:
        ids = load_page_ids(target, id_cache)
        if fragment not in ids:
            rel = target.relative_to(SITE_ROOT)
            return LinkIssue(
                ref.source,
                ref.raw,
                f"missing anchor #{fragment} in {rel.as_posix()}",
                ref.line,
            )
    return None


def run(*, check_external: bool) -> list[LinkIssue]:
    id_cache: dict[Path, set[str]] = {}
    issues: list[LinkIssue] = []
    for html_path in iter_html_files():
        for ref in collect_links(html_path):
            issue = check_link(ref, check_external=check_external, id_cache=id_cache)
            if issue:
                issues.append(issue)
    return issues


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--external",
        action="store_true",
        help="Also verify http(s) links with HEAD requests (manual use; slow and network-dependent).",
    )
    args = parser.parse_args(argv)

    issues = run(check_external=args.external)
    if not issues:
        print(f"link check passed ({len(iter_html_files())} HTML file(s))")
        return 0

    for issue in issues:
        rel = issue.source.relative_to(ROOT)
        loc = f"{rel.as_posix()}"
        if issue.line:
            loc = f"{loc}:{issue.line}"
        print(f"{loc}: {issue.raw!r} — {issue.detail}", file=sys.stderr)
    print(f"link check failed ({len(issues)} issue(s))", file=sys.stderr)
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
