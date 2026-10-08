"""Build a grit-lib API map table from local rustdoc HTML."""
from __future__ import annotations

import html
import re
import sys
from dataclasses import dataclass
from pathlib import Path

import rustdoc_links

API_MAP_MARKER = "<!-- grit:api-map -->"
ITEM_KINDS = ("struct", "enum", "trait")
MOD_LINK_RE = re.compile(
    r'<a class="mod" href="([^"]+)" title="mod ([^"]+)">',
)
ITEM_ROW_RE = re.compile(
    r'<a class="(struct|enum|trait)" href="([^"]+)" title="(?:struct|enum|trait) ([^"]+)">'
    r"[^<]*</a></dt><dd>(.*?)</dd>",
    re.DOTALL,
)
TOP_DOC_FIRST_P_RE = re.compile(
    r'class="toggle top-doc"[^>]*>.*?<div class="docblock">.*?<p>(.*?)</p>',
    re.DOTALL,
)


@dataclass(frozen=True)
class ApiRow:
    """One row in the generated API map (module or type)."""

    qualified: str
    kind: str
    summary: str
    docs_rs_path: str


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def strip_html(fragment: str) -> str:
    text = re.sub(r"<code[^>]*>(.*?)</code>", r"\1", fragment, flags=re.DOTALL)
    text = re.sub(r"<[^>]+>", "", text)
    return html.unescape(text).strip()


def first_sentence(text: str) -> str:
    text = " ".join(text.split())
    if not text:
        return ""
    match = re.match(r"^(.+?[.!?])(?:\s|$)", text)
    if match:
        return match.group(1)
    return text


def module_index_paths(crate_dir: Path) -> list[Path]:
    """Return every public module ``index.html`` under ``grit_lib/`` (not the crate root)."""
    discovered: set[Path] = set()
    pending: list[Path] = []

    crate_index = crate_dir / "index.html"
    if not crate_index.is_file():
        raise SystemExit(f"grit-lib rustdoc missing crate index {crate_index}")

    for href, _title in MOD_LINK_RE.findall(crate_index.read_text(encoding="utf-8")):
        pending.append((crate_dir / href).resolve())

    while pending:
        index_path = pending.pop()
        if index_path in discovered or not index_path.is_file():
            continue
        discovered.add(index_path)
        text = index_path.read_text(encoding="utf-8")
        parent = index_path.parent
        for rel, _qualified in MOD_LINK_RE.findall(text):
            pending.append((parent / rel).resolve())

    return sorted(discovered)


def qualified_from_title(title: str) -> str:
    if not title.startswith("mod "):
        raise ValueError(f"expected mod title, got {title!r}")
    return title.removeprefix("mod ")


def docs_path_for_index(index_path: Path, crate_dir: Path) -> str:
    rel = index_path.relative_to(crate_dir)
    return f"grit_lib/{rel.as_posix()}"


def docs_path_for_item(index_path: Path, item_href: str, crate_dir: Path) -> str:
    rel = (index_path.parent / item_href).relative_to(crate_dir)
    return f"grit_lib/{rel.as_posix()}"


def module_summary_from_index(text: str) -> str | None:
    match = TOP_DOC_FIRST_P_RE.search(text)
    if not match:
        return None
    paragraph = strip_html(match.group(1))
    return first_sentence(paragraph) if paragraph else None


def parse_module_index(index_path: Path, crate_dir: Path) -> tuple[ApiRow, list[ApiRow], list[str]]:
    text = index_path.read_text(encoding="utf-8")
    mod_titles = MOD_LINK_RE.findall(text)
    # Module pages use breadcrumbs; nested mods carry title on child links only.
    qualified = None
    for _href, title in mod_titles:
        if title.startswith("mod grit_lib::"):
            qualified = title.removeprefix("mod ")
            break
    if qualified is None:
        title_match = re.search(
            r'<title>(grit_lib(?:::[A-Za-z0-9_]+)+) - Rust</title>',
            text,
        )
        if title_match:
            qualified = title_match.group(1)
    if qualified is None:
        rel = index_path.relative_to(crate_dir).parent.as_posix()
        qualified = "grit_lib" if rel == "." else f"grit_lib::{rel.replace('/', '::')}"

    summary = module_summary_from_index(text)
    missing: list[str] = []
    if not summary:
        missing.append(qualified)

    module_row = ApiRow(
        qualified=qualified,
        kind="module",
        summary=summary or "",
        docs_rs_path=docs_path_for_index(index_path, crate_dir),
    )

    items: list[ApiRow] = []
    for kind, href, title, dd in ITEM_ROW_RE.findall(text):
        qualified_item = title.removeprefix(f"{kind} ")
        items.append(
            ApiRow(
                qualified=qualified_item,
                kind=kind,
                summary=first_sentence(strip_html(dd)),
                docs_rs_path=docs_path_for_item(index_path, href, crate_dir),
            )
        )
    items.sort(key=lambda row: (row.kind, row.qualified))
    return module_row, items, missing


def build_rows(doc_root: Path) -> tuple[tuple[ApiRow, ...], list[str]]:
    crate_dir = doc_root / "grit_lib"
    if not crate_dir.is_dir():
        raise SystemExit(
            f"grit-lib rustdoc not found at {crate_dir}; run: cargo doc -p grit-lib --no-deps"
        )

    rows: list[ApiRow] = []
    missing: list[str] = []
    for index_path in module_index_paths(crate_dir):
        module_row, item_rows, module_missing = parse_module_index(index_path, crate_dir)
        missing.extend(module_missing)
        rows.append(module_row)
        rows.extend(item_rows)

    if missing:
        missing_sorted = sorted(set(missing))
        raise SystemExit(
            "grit-lib modules missing `//!` summary (add module docs, then run make doc):\n  "
            + "\n  ".join(missing_sorted)
        )

    return tuple(rows), missing


def render_markdown_table(rows: tuple[ApiRow, ...]) -> str:
    lines = [
        "| Item | Kind | Summary | docs.rs |",
        "| --- | --- | --- | --- |",
    ]
    for row in rows:
        label = f"`{row.qualified}`"
        url = rustdoc_links.docs_rs_url(row.docs_rs_path)
        summary = row.summary.replace("|", "\\|")
        lines.append(f"| {label} | {row.kind} | {summary} | [API]({url}) |")
    return "\n".join(lines) + "\n"


def api_map_markdown(doc_root: Path) -> str:
    rows, _missing = build_rows(doc_root)
    return render_markdown_table(rows)


def split_api_map_markdown(markdown: str) -> tuple[str, str]:
    if API_MAP_MARKER not in markdown:
        raise SystemExit(f"API map page must contain marker {API_MAP_MARKER}")
    before, after = markdown.split(API_MAP_MARKER, 1)
    return before, after


def main(argv: list[str] | None = None) -> int:
    doc_root = repo_root() / "target" / "doc"
    sys.stdout.write(api_map_markdown(doc_root))
    if argv is not None:
        del argv
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
