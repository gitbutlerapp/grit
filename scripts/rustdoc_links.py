"""Resolve ``rustdoc:`` links in docs Markdown to docs.rs URLs and validate against local rustdoc."""
from __future__ import annotations

import re
from pathlib import Path

DOCS_RS_BASE = "https://docs.rs/grit-lib/latest"
RUSTDOC_LINK_IN_MARKDOWN = re.compile(r"\]\(rustdoc:([^)]+)\)")
RUSTDOC_PATH = re.compile(r"^grit_lib(::[A-Za-z_][A-Za-z0-9_]*)*$")

ITEM_KIND_PREFIXES = (
    "struct",
    "enum",
    "fn",
    "trait",
    "type",
    "constant",
    "union",
    "macro",
    "mod",
)


def docs_rs_url(relative_html: str) -> str:
    """Build a docs.rs URL from a path under ``grit_lib/`` (e.g. ``repo/struct.Repository.html``)."""
    return f"{DOCS_RS_BASE}/{relative_html.lstrip('/')}"


def local_html_to_docs_rs_path(local: Path, doc_root: Path) -> str:
    """Return the docs.rs path segment (``grit_lib/...``) for a file under ``target/doc``."""
    grit_lib_root = (doc_root / "grit_lib").resolve()
    rel = local.resolve().relative_to(grit_lib_root)
    return f"grit_lib/{rel.as_posix()}"


def resolve_local_rustdoc_html(doc_root: Path, qualified: str) -> Path:
    """Find the rustdoc HTML file for a path like ``grit_lib::repo::Repository``."""
    if not RUSTDOC_PATH.fullmatch(qualified):
        raise ValueError(f"invalid rustdoc path syntax: {qualified!r}")

    parts = qualified.split("::")
    crate_dir = doc_root / "grit_lib"
    if not crate_dir.is_dir():
        raise FileNotFoundError(f"local rustdoc missing crate dir {crate_dir}")

    if len(parts) == 1:
        index = crate_dir / "index.html"
        if index.is_file():
            return index
        raise FileNotFoundError(qualified)

    current = crate_dir
    segments = parts[1:]
    *module_parts, last = segments

    for mod_name in module_parts:
        nested = current / mod_name
        if (nested / "index.html").is_file():
            current = nested
            continue
        mod_page = current / f"mod.{mod_name}.html"
        if mod_page.is_file():
            current = nested if nested.is_dir() else current / mod_name
            continue
        raise FileNotFoundError(f"rustdoc module not found: {'::'.join(parts[: parts.index(mod_name) + 2])}")

    for prefix in ITEM_KIND_PREFIXES:
        candidate = current / f"{prefix}.{last}.html"
        if candidate.is_file():
            return candidate

    mod_index = current / last / "index.html"
    if mod_index.is_file():
        return mod_index

    raise FileNotFoundError(qualified)


def expand_rustdoc_links(markdown: str, *, doc_root: Path | None) -> str:
    """Replace ``(rustdoc:grit_lib::...)`` link targets with docs.rs URLs."""

    def replace(match: re.Match[str]) -> str:
        qualified = match.group(1).strip()
        if doc_root is not None and (doc_root / "grit_lib").is_dir():
            local = resolve_local_rustdoc_html(doc_root, qualified)
            url = docs_rs_url(local_html_to_docs_rs_path(local, doc_root))
        else:
            # Best-effort URL without local validation (struct prefix guess).
            tail = qualified.replace("::", "/")
            if "::" in qualified:
                name = qualified.rsplit("::", 1)[-1]
                url = f"{DOCS_RS_BASE}/grit_lib/{tail.rsplit('/', 1)[0]}/struct.{name}.html"
            else:
                url = f"{DOCS_RS_BASE}/grit_lib/index.html"
        return f"]({url})"

    return RUSTDOC_LINK_IN_MARKDOWN.sub(replace, markdown)


def iter_rustdoc_paths(markdown: str) -> list[str]:
    return [m.group(1).strip() for m in RUSTDOC_LINK_IN_MARKDOWN.finditer(markdown)]


def validate_rustdoc_links(content_root: Path, doc_root: Path) -> None:
    """Fail if any ``rustdoc:`` link under ``content_root`` does not resolve in ``doc_root``."""
    if not (doc_root / "grit_lib").is_dir():
        return

    missing: list[str] = []
    for path in sorted(content_root.rglob("*.md")):
        text = path.read_text(encoding="utf-8")
        for qualified in iter_rustdoc_paths(text):
            try:
                resolve_local_rustdoc_html(doc_root, qualified)
            except (FileNotFoundError, ValueError) as err:
                rel = path.relative_to(content_root)
                missing.append(f"{rel}: rustdoc:{qualified} ({err})")

    if missing:
        raise SystemExit(
            "unresolved rustdoc links (build grit-lib docs first):\n  "
            + "\n  ".join(missing)
        )
